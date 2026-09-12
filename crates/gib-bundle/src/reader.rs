//! Reading a bundle back: the header's refs, and every object in the packfile
//! behind it.
//!
//! This is the other direction from [`writer`](crate::writer), and it is the
//! harder one, because a bundle that arrives here was written by someone else —
//! `git bundle create` deltifies aggressively, and the file itself came off a
//! user's disk rather than from the server the rest of the viewer talks to. So
//! nothing here is taken on trust: the pack is walked once from front to back,
//! each object inflated, deltas rebuilt against what came before, and every id
//! computed from the bytes rather than believed. An object that doesn't hash to
//! a name simply never gets that name.
//!
//! Objects leave through [`ObjectStore`] as they are read, one at a time, so a
//! repository's worth of history is never held here at once. The store is also
//! where a delta's base is read back from when it has aged out of the window
//! below — every base is an object the walk has already handed over, so the
//! thing being filled is also what makes filling it possible.

use crate::BundleRef;
use futures::future::LocalBoxFuture;
use gib_object::{ObjectId, RawObject};
use gib_pack::{PackObjectType, apply_delta, parse_object_header};
use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{
    DecompressorOxide, decompress,
    inflate_flags::{TINFL_FLAG_PARSE_ZLIB_HEADER, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF},
};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

/// How much object content to keep around as candidate delta bases.
///
/// A delta's base is nearly always a few objects back — that is what the window
/// a pack was written with means — so holding a fraction of a repository is
/// enough to rebuild almost every delta without reading anything back, and the
/// rare long reach falls through to [`ObjectStore::get`] rather than failing.
const WINDOW_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// The largest single object this will inflate.
///
/// The size comes out of the pack's own header, before a byte of the object has
/// been read, so a crafted (or merely truncated) one would otherwise have us
/// allocate whatever it asks for. An object larger than a whole bundle may be
/// can't be inside one — see [`MAX_BUNDLE_BYTES`](crate::MAX_BUNDLE_BYTES).
const MAX_OBJECT_BYTES: u64 = crate::MAX_BUNDLE_BYTES as u64;

/// `PACK`, the version, and the object count.
const PACK_HEADER_LEN: usize = 12;

/// The SHA-1 over the packfile's bytes that closes it.
const PACK_TRAILER_LEN: usize = 20;

/// Where a bundle's objects go as they are read, and where a delta's base is
/// read back from when the window no longer holds it.
pub trait ObjectStore {
    /// Take one object. Called once per object in the pack, in pack order.
    fn put(&self, id: ObjectId, object: Rc<RawObject>) -> LocalBoxFuture<'_, anyhow::Result<()>>;

    /// Read an object back, or `None` when the store doesn't have it. Only
    /// reached for a delta base that has aged out of the window, or one the
    /// bundle names as a prerequisite and so never carried at all.
    fn get(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Option<RawObject>>>;
}

/// What a bundle says it carries, read off the lines above its packfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHeader {
    /// 2 or 3; only a version 3 bundle carries capability lines.
    pub version: u32,
    /// The refs the bundle names, and the objects they point at.
    pub refs: Vec<BundleRef>,
    /// Objects the bundle expects the reader to have already: a differential
    /// bundle starts from these rather than carrying them.
    pub prerequisites: Vec<ObjectId>,
    /// Where the packfile begins, as an index into the bundle.
    pub pack_offset: usize,
}

/// What reading one produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSummary {
    /// The header the pack sat behind.
    pub header: BundleHeader,
    /// How many objects were read out of the pack and handed to the store.
    pub objects: usize,
}

/// Read a bundle's header: the signature line, the refs and prerequisites under
/// it, and where the packfile behind them starts.
pub fn read_header(bundle: &[u8]) -> anyhow::Result<BundleHeader> {
    let (signature, mut rest) = split_line(bundle).ok_or_else(|| not_a_bundle(bundle))?;
    let version = match signature {
        b"# v2 git bundle" => 2,
        b"# v3 git bundle" => 3,
        _ => return Err(not_a_bundle(bundle)),
    };

    let mut refs = Vec::new();
    let mut prerequisites = Vec::new();
    loop {
        let (line, tail) = split_line(rest)
            .ok_or_else(|| anyhow::anyhow!("the bundle header has no blank line ending it"))?;
        rest = tail;
        // The blank line ends the header; the pack starts on the next byte.
        if line.is_empty() {
            return Ok(BundleHeader {
                version,
                refs,
                prerequisites,
                pack_offset: bundle.len() - rest.len(),
            });
        }

        // Only a v3 bundle has capabilities. In a v2 one a leading '@' is
        // simply a line that names no object, which is what the read below
        // will say about it.
        if version == 3
            && let Some(capability) = line.strip_prefix(b"@")
        {
            read_capability(capability)?;
            continue;
        }

        // A prerequisite is an object id followed by an optional subject line;
        // a ref line is an object id followed by the ref's name.
        match line.strip_prefix(b"-") {
            Some(prerequisite) => prerequisites.push(read_id(prerequisite)?.0),
            None => {
                let (id, name) = read_id(line)?;
                let name = name.ok_or_else(|| {
                    anyhow::anyhow!("a bundle header line names no ref: {}", show(line))
                })?;
                let name = String::from_utf8(name.to_vec())
                    .map_err(|_| anyhow::anyhow!("the bundle names a ref that isn't UTF-8"))?;
                refs.push(BundleRef::new(name, id));
            }
        }
    }
}

/// Read a whole bundle: the header, then every object in the packfile, each
/// handed to `store` as it is rebuilt.
///
/// `on_progress` is called with how many objects have been read out of how many
/// the pack says it holds — a real denominator, unlike the walk that writes
/// one, because a pack states its object count up front.
pub async fn read_bundle<S: ObjectStore>(
    bundle: &[u8],
    store: &S,
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<BundleSummary> {
    let header = read_header(bundle)?;
    let pack = &bundle[header.pack_offset..];
    let total = verify_pack(pack)?;
    on_progress(0, total);

    let mut reader = PackReader {
        store,
        seen: BTreeMap::new(),
        window: BTreeMap::new(),
        order: VecDeque::new(),
        window_bytes: 0,
    };

    let mut pos = PACK_HEADER_LEN;
    for done in 0..total {
        pos = reader.read_object(pack, pos).await?;
        on_progress(done + 1, total);
    }

    // Bytes between the last object and the closing checksum are an object the
    // count didn't mention: the pack and its own header disagree about what is
    // in here, and the objects already stored are the ones it did account for.
    if pos != pack.len() - PACK_TRAILER_LEN {
        anyhow::bail!(
            "the packfile holds {} bytes its {total} objects don't account for",
            pack.len() - PACK_TRAILER_LEN - pos
        );
    }

    Ok(BundleSummary {
        header,
        objects: total,
    })
}

/// The walk's state: what it has read, and the bodies recent enough that a
/// delta might be written against them.
struct PackReader<'a, S: ObjectStore> {
    store: &'a S,
    /// Every object read so far, by the offset it sits at — which is how an
    /// `OFS_DELTA` names its base, and the only way to turn that back into a
    /// name the store knows the object by.
    seen: BTreeMap<usize, ObjectId>,
    /// Recently read objects, held as delta bases.
    window: BTreeMap<ObjectId, Rc<RawObject>>,
    /// The ids in `window`, oldest first, so the oldest can be dropped.
    order: VecDeque<ObjectId>,
    /// What `window` holds, in bytes of object content.
    window_bytes: usize,
}

impl<S: ObjectStore> PackReader<'_, S> {
    /// Read the object at `pos`, hand it to the store, and answer with where
    /// the next one starts.
    async fn read_object(&mut self, pack: &[u8], pos: usize) -> anyhow::Result<usize> {
        // Everything from here to the checksum: an object may not reach into
        // the trailer, and nothing says where this one ends until it is
        // inflated.
        let rest = pack
            .get(pos..pack.len() - PACK_TRAILER_LEN)
            .ok_or_else(|| anyhow::anyhow!("the packfile ends mid-object"))?;
        let (object_type, size, header_len) = parse_object_header(rest)
            .map_err(|e| anyhow::anyhow!("malformed object header at {pos}: {e:?}"))?;
        if size.0 > MAX_OBJECT_BYTES {
            anyhow::bail!(
                "the bundle claims a {}-byte object, larger than a bundle may be",
                size.0
            );
        }
        let size = usize::try_from(size.0)
            .map_err(|_| anyhow::anyhow!("the bundle claims an object too large to address"))?;

        let (stored, consumed) = inflate(&rest[header_len..], size)
            .map_err(|e| anyhow::anyhow!("the object at {pos} did not inflate: {e}"))?;
        let next = pos + header_len + consumed;

        let object = match object_type {
            PackObjectType::Base(object_type) => RawObject {
                object_type,
                body: stored,
            },
            PackObjectType::OffsetDelta { base_offset_neg } => {
                let distance = usize::try_from(base_offset_neg.0).unwrap_or(usize::MAX);
                let base_at = pos.checked_sub(distance).ok_or_else(|| {
                    anyhow::anyhow!("the delta at {pos} names a base before the packfile")
                })?;
                let base_id = *self.seen.get(&base_at).ok_or_else(|| {
                    anyhow::anyhow!("the delta at {pos} names offset {base_at}, which is not one")
                })?;
                self.rebuild(base_id, &stored).await?
            }
            PackObjectType::RefDelta { base_id } => self.rebuild(base_id, &stored).await?,
        };

        // The name is computed, never taken on the bundle's word: this is where
        // objects out of a file someone picked enter the store, and one stored
        // under the id of its own bytes cannot be passed off as another object.
        let object = Rc::new(object);
        let id = object.compute_id();
        self.seen.insert(pos, id);
        self.remember(id, Rc::clone(&object));
        self.store.put(id, object).await?;
        Ok(next)
    }

    /// Rebuild a delta against the object it was written against, wherever that
    /// has got to: still in the window, or already in the store.
    async fn rebuild(&self, base_id: ObjectId, delta: &[u8]) -> anyhow::Result<RawObject> {
        let base = match self.window.get(&base_id) {
            Some(base) => Rc::clone(base),
            None => Rc::new(self.store.get(base_id).await?.ok_or_else(|| {
                anyhow::anyhow!("the bundle is a delta against {base_id}, which it doesn't carry")
            })?),
        };
        let body = apply_delta(delta, &base.body)
            .map_err(|e| anyhow::anyhow!("a delta against {base_id} is malformed: {e:?}"))?;
        Ok(RawObject {
            object_type: base.object_type,
            body,
        })
    }

    /// Keep an object as a candidate delta base, dropping the oldest ones once
    /// the window is over its budget.
    fn remember(&mut self, id: ObjectId, object: Rc<RawObject>) {
        self.window_bytes += object.body.len();
        if self.window.insert(id, object).is_none() {
            self.order.push_back(id);
        }
        while self.window_bytes > WINDOW_MEMORY_BYTES && self.order.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(dropped) = self.window.remove(&oldest) {
                self.window_bytes -= dropped.body.len();
            }
        }
    }
}

/// Check a packfile over: that it is one we can read, that its bytes are the
/// ones it was written with, and how many objects it says it holds.
fn verify_pack(pack: &[u8]) -> anyhow::Result<usize> {
    if pack.len() < PACK_HEADER_LEN + PACK_TRAILER_LEN {
        anyhow::bail!("the bundle has no packfile behind its header");
    }
    if &pack[..4] != b"PACK" {
        anyhow::bail!("what follows the bundle's header is not a packfile");
    }
    let version = u32::from_be_bytes(pack[4..8].try_into().unwrap());
    if version != 2 {
        anyhow::bail!("the bundle's packfile is version {version}, not 2");
    }

    // A pack ends with the SHA-1 of everything before it. Checking that here
    // means a truncated or damaged file is refused before a single object out
    // of it is stored, rather than half-imported and then found out.
    let (body, trailer) = pack.split_at(pack.len() - PACK_TRAILER_LEN);
    let mut hasher = Sha1::new();
    hasher.update(body);
    if hasher.finalize().as_slice() != trailer {
        anyhow::bail!("the bundle's packfile is damaged: its checksum doesn't match its contents");
    }

    Ok(u32::from_be_bytes(pack[8..12].try_into().unwrap()) as usize)
}

/// Inflate one object's body, which is `size` bytes once it is out, and say how
/// much of `input` that took — the next object starts right after.
///
/// `input` runs to the end of the pack rather than to the end of this object,
/// because nothing says where that is until the zlib stream ends. Deliberately
/// not claiming there is more input coming is what makes a stream that stops
/// early come back as the error it is rather than as a wait for more.
fn inflate(input: &[u8], size: usize) -> Result<(Vec<u8>, usize), String> {
    let mut body = vec![0u8; size];
    let mut state = Box::<DecompressorOxide>::default();
    let (status, consumed, written) = decompress(
        &mut state,
        input,
        &mut body,
        0,
        // Parsing the zlib header is also what has the checksum over the
        // object's bytes checked, which is the one integrity claim a pack makes
        // per object rather than over the file as a whole.
        TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    if status != TINFLStatus::Done {
        // Anything the object's own header didn't account for: more bytes than
        // it said there would be, a stream that stops early, or a checksum that
        // doesn't match what came out.
        return Err(format!("{status:?}"));
    }
    if written != size {
        return Err(format!(
            "inflated to {written} bytes, not the {size} claimed"
        ));
    }
    Ok((body, consumed))
}

/// One line, and everything after it. The line excludes its newline; `None`
/// when there isn't one, which in a header means it ran off the end.
fn split_line(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = bytes.iter().position(|&b| b == b'\n')?;
    Some((&bytes[..end], &bytes[end + 1..]))
}

/// A `@`-prefixed line in a v3 bundle. The only one we can honour is the hash
/// algorithm, and only where it is the one this implements.
fn read_capability(capability: &[u8]) -> anyhow::Result<()> {
    match capability {
        b"object-format=sha1" => Ok(()),
        other => anyhow::bail!(
            "the bundle needs a capability this viewer doesn't have: {}",
            show(other)
        ),
    }
}

/// An object id at the start of a line, and whatever followed the space after
/// it — a ref's name on a ref line, a subject on a prerequisite.
fn read_id(line: &[u8]) -> anyhow::Result<(ObjectId, Option<&[u8]>)> {
    let (hex, rest) = match line.iter().position(|&b| b == b' ') {
        Some(space) => (&line[..space], Some(&line[space + 1..])),
        None => (line, None),
    };
    let id = ObjectId::from_hex(hex)
        .ok_or_else(|| anyhow::anyhow!("a bundle header line names no object: {}", show(line)))?;
    Ok((id, rest.filter(|r| !r.is_empty())))
}

/// A header line as something safe to put in an error message.
fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(64)]).into_owned()
}

fn not_a_bundle(bundle: &[u8]) -> anyhow::Error {
    if bundle.is_empty() {
        anyhow::anyhow!("the file is empty")
    } else {
        anyhow::anyhow!("not a v2 or v3 git bundle: {}", show(bundle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BundleWriter;
    use crate::test_support::MemoryStore;
    use futures::executor::block_on;
    use gib_object::ObjectType;
    use miniz_oxide::deflate::compress_to_vec_zlib;

    /// A bundle over `objects`, named by a ref pointing at the last of them —
    /// written by the writer this crate already has, which is the only thing
    /// around that produces one without a `git` to run.
    fn bundle(objects: &[(ObjectType, Vec<u8>)]) -> Vec<u8> {
        let last = RawObject {
            object_type: objects.last().unwrap().0,
            body: objects.last().unwrap().1.clone(),
        };
        let mut writer = BundleWriter::new(
            &[BundleRef::new("refs/heads/main", last.compute_id())],
            objects.len(),
        )
        .expect("a writer");
        for (object_type, body) in objects {
            writer.append(*object_type, body).expect("an object");
        }
        writer.finish().expect("a bundle")
    }

    fn read(bundle: &[u8]) -> (MemoryStore, anyhow::Result<BundleSummary>) {
        let store = MemoryStore::default();
        let summary = block_on(read_bundle(bundle, &store, &|_, _| {}));
        (store, summary)
    }

    /// A blob big enough, and a second one similar enough, that the writer
    /// stores the second as a delta against the first: the read back then has
    /// to rebuild it rather than just inflate it.
    fn similar_blobs() -> [(ObjectType, Vec<u8>); 2] {
        let first: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let mut second = first.clone();
        second.extend_from_slice(b"and a little more\n");
        [(ObjectType::Blob, first), (ObjectType::Blob, second)]
    }

    #[test]
    fn reads_back_every_object_a_bundle_carries() {
        let objects = [
            (ObjectType::Blob, b"one\n".to_vec()),
            (ObjectType::Blob, b"two\n".to_vec()),
        ];
        let (store, summary) = read(&bundle(&objects));
        let summary = summary.expect("the bundle reads");

        assert_eq!(summary.objects, 2);
        assert_eq!(store.len(), 2);
        for (object_type, body) in &objects {
            let id = RawObject {
                object_type: *object_type,
                body: body.clone(),
            }
            .compute_id();
            assert_eq!(store.body(id).as_ref(), Some(body));
            assert_eq!(store.object_type(id), Some(*object_type));
        }
    }

    /// The refs and prerequisites are read off the header, not guessed at from
    /// the pack.
    #[test]
    fn reads_the_refs_a_bundle_names() {
        let (_, summary) = read(&bundle(&[(ObjectType::Blob, b"one\n".to_vec())]));
        let header = summary.expect("the bundle reads").header;

        assert_eq!(header.version, 2);
        assert_eq!(header.refs.len(), 1);
        assert_eq!(header.refs[0].name, "refs/heads/main");
        assert!(header.prerequisites.is_empty());
    }

    /// A delta written against an object earlier in the same pack, which is how
    /// all but the first revision of a file is stored.
    #[test]
    fn rebuilds_a_delta_against_the_pack() {
        let objects = similar_blobs();
        let bundle = bundle(&objects);
        let (store, summary) = read(&bundle);
        summary.expect("the bundle reads");

        for (object_type, body) in &objects {
            let id = RawObject {
                object_type: *object_type,
                body: body.clone(),
            }
            .compute_id();
            assert_eq!(
                store.body(id).as_ref(),
                Some(body),
                "object {id} came back wrong"
            );
        }
        // The base was still in the window, so nothing was read back out of the
        // store to rebuild it.
        assert_eq!(store.reads(), 0);
    }

    /// The size of the pack is not the measure of what was carried: a delta
    /// against a base the bundle doesn't have is read back from the store,
    /// which is how a differential bundle's thin pack rebuilds.
    #[test]
    fn rebuilds_a_delta_against_an_object_the_store_already_has() {
        let store = MemoryStore::default();
        let base = b"the base object\n".to_vec();
        let base_id = store.seed(ObjectType::Blob, base.clone());

        let tail = b"with a tail\n";
        let bundle = ref_delta_bundle(base_id, &base, tail);
        let summary = block_on(read_bundle(&bundle, &store, &|_, _| {})).expect("it reads");

        assert_eq!(summary.objects, 1);
        let mut expected = base.clone();
        expected.extend_from_slice(tail);
        let rebuilt = RawObject {
            object_type: ObjectType::Blob,
            body: expected.clone(),
        }
        .compute_id();
        assert_eq!(store.body(rebuilt), Some(expected));
        assert_eq!(store.reads(), 1, "the base was not read back");
    }

    /// The same bundle with nothing seeded: the base is nowhere, and that is an
    /// error naming the object the reader would have needed.
    #[test]
    fn a_delta_with_no_base_anywhere_is_an_error() {
        let base = b"the base object\n".to_vec();
        let base_id = RawObject {
            object_type: ObjectType::Blob,
            body: base.clone(),
        }
        .compute_id();

        let (_, summary) = read(&ref_delta_bundle(base_id, &base, b"with a tail\n"));
        let err = summary.expect_err("the bundle does not read");
        assert!(err.to_string().contains(&base_id.to_string()), "{err}");
    }

    /// A bundle whose pack holds one `REF_DELTA` against `base_id`, appending
    /// `tail` to `base`. Hand-assembled because the writer only ever deltifies
    /// against the pack it is writing — a thin pack is something only `git`
    /// produces, and this is the shape it produces.
    fn ref_delta_bundle(base_id: ObjectId, base: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut delta = Vec::new();
        write_size(&mut delta, base.len());
        write_size(&mut delta, base.len() + tail.len());
        // Copy the whole base, then append the tail: one instruction each.
        delta.extend_from_slice(&[0b1001_0001, 0x00, base.len() as u8]);
        delta.push(tail.len() as u8);
        delta.extend_from_slice(tail);

        let mut pack = Vec::from(*b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        // Type 7 (`REF_DELTA`) and the delta's own size, then the base's id.
        let mut header = vec![0b0111_0000 | (delta.len() as u8 & 0x0f)];
        let mut size = delta.len() >> 4;
        while size > 0 {
            *header.last_mut().unwrap() |= 0b1000_0000;
            header.push((size & 0x7f) as u8);
            size >>= 7;
        }
        pack.extend_from_slice(&header);
        pack.extend_from_slice(base_id.bytes());
        pack.extend_from_slice(&compress_to_vec_zlib(&delta, 6));
        let mut hasher = Sha1::new();
        hasher.update(&pack);
        pack.extend_from_slice(&hasher.finalize());

        let mut out = Vec::from(*b"# v2 git bundle\n");
        out.extend_from_slice(format!("{base_id} refs/heads/main\n\n").as_bytes());
        out.extend_from_slice(&pack);
        out
    }

    /// A delta's two sizes, in the little-endian varint the delta header uses.
    fn write_size(out: &mut Vec<u8>, mut size: usize) {
        loop {
            let byte = (size & 0x7f) as u8;
            size >>= 7;
            out.push(if size > 0 { byte | 0x80 } else { byte });
            if size == 0 {
                return;
            }
        }
    }

    #[test]
    fn reads_a_v3_header_with_the_hash_it_names() {
        let mut bundle = bundle(&[(ObjectType::Blob, b"one\n".to_vec())]);
        let pack_at = read_header(&bundle).unwrap().pack_offset;
        let mut v3 = Vec::from(*b"# v3 git bundle\n@object-format=sha1\n");
        v3.extend_from_slice(&bundle[16..]);
        bundle = v3;

        let header = read_header(&bundle).expect("a v3 header");
        assert_eq!(header.version, 3);
        assert_eq!(header.refs.len(), 1);
        // The capability line moved the pack along by exactly its own length.
        assert_eq!(header.pack_offset, pack_at + "@object-format=sha1\n".len());
        read(&bundle).1.expect("and it still reads");
    }

    /// A bundle in a hash this viewer doesn't implement is refused by name,
    /// rather than read as if every id in it were a SHA-1.
    #[test]
    fn refuses_a_capability_it_cannot_honour() {
        let bundle = b"# v3 git bundle\n@object-format=sha256\n\n".to_vec();
        let err = read_header(&bundle).expect_err("not readable");
        assert!(err.to_string().contains("object-format=sha256"), "{err}");
    }

    #[test]
    fn reads_the_prerequisites_a_differential_bundle_names() {
        let id = "1234567890123456789012345678901234567890";
        let bundle = format!("# v2 git bundle\n-{id} one\n{id} refs/heads/main\n\n");
        let header = read_header(bundle.as_bytes()).expect("a header");

        assert_eq!(header.prerequisites.len(), 1);
        assert_eq!(header.prerequisites[0].to_string(), id);
        assert_eq!(header.refs.len(), 1);
        assert_eq!(header.refs[0].name, "refs/heads/main");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_bundle() {
        let err = read_header(b"PK\x03\x04 not a bundle at all").expect_err("not a bundle");
        assert!(
            err.to_string().contains("not a v2 or v3 git bundle"),
            "{err}"
        );
        let err = read_header(b"").expect_err("not a bundle");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// The pack's closing checksum is over its own bytes, so a file that was
    /// damaged anywhere in it is refused — and refused before anything out of
    /// it has been stored.
    #[test]
    fn refuses_a_damaged_pack_before_storing_any_of_it() {
        let mut bundle = bundle(&similar_blobs());
        let last = bundle.len() - 32;
        bundle[last] ^= 0xff;

        let (store, summary) = read(&bundle);
        let err = summary.expect_err("the bundle does not read");
        assert!(err.to_string().contains("checksum"), "{err}");
        assert_eq!(store.len(), 0, "a damaged bundle left objects behind");
    }

    /// A file that stops in the middle of its pack. The checksum can't match
    /// what isn't there, so this is caught for the same reason, but it is the
    /// likelier accident of the two: an upload that didn't finish.
    #[test]
    fn refuses_a_truncated_bundle() {
        let bundle = bundle(&similar_blobs());
        let (store, summary) = read(&bundle[..bundle.len() - 64]);
        assert!(summary.is_err(), "a truncated bundle read as a whole one");
        assert_eq!(store.len(), 0);
    }

    /// Progress is reported against the pack's own object count, and ends on it.
    #[test]
    fn reports_progress_against_the_object_count() {
        let store = MemoryStore::default();
        let seen = std::cell::RefCell::new(Vec::new());
        let bundle = bundle(&similar_blobs());
        block_on(read_bundle(&bundle, &store, &|done, total| {
            seen.borrow_mut().push((done, total))
        }))
        .expect("the bundle reads");

        assert_eq!(*seen.borrow(), vec![(0, 2), (1, 2), (2, 2)]);
    }
}
