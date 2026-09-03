# Where gib's git differs from git's git

A review of the `gib-*` crates against the C source of git itself.

**Oracle:** `../git` at `1a3e64c6c4a6` (2026-08-20). Line references of the form
`packfile.c:119` are into that checkout; references of the form
`crates/…/src/x.rs:12` are into this one.

**Method:** read git's implementation of each format and operation, then read
ours; where the answer wasn't obvious from the source, a throwaway probe was
run against the host `git` binary. Items proven by such a probe are marked
**[probed]**; the probes were deleted afterwards, so anything marked that way is
worth turning into a real test.

Every crate's existing `differential.rs` passes; nothing below is a regression.
These are the gaps *outside* the fixtures those suites use.

As things are fixed, remove them from this list.

---

## Summary

| # | Area | Difference | Kind |
|---|---|---|---|
| 1 | objects | Truncated header block panics instead of erroring | **bug** |
| 2 | objects | Broken author/committer idents reject the whole object | **bug** |
| 3 | objects | An object with no `author`/`committer` is rejected | robustness |
| 4 | objects | Unrecognised tree mode rejects the whole tree | robustness |
| 5 | objects | Empty tree-entry filenames accepted | leniency |
| 8 | patch | Non-UTF-8 bytes replaced with U+FFFD | **wrong output** |
| 10 | patch | `XDF_INDENT_HEURISTIC` not set; different hunk placement | fidelity |
| 11 | patch | No gitattributes (`diff`, `-diff`, `binary`, `text`) | missing |
| 12 | pack | Pack v3 and index v1 rejected | missing |
| 13 | pack | Corrupt deltas panic instead of erroring | **bug** |
| 14 | pack | Delta opcode 0 silently ignored | correctness |
| 15 | pack | No offset-delta bound check; cyclic chains hang | **bug** |
| 16 | pack | Truncated object headers panic | **bug** |
| 17 | odb | No `objects/info/alternates` | missing |
| 18 | odb | No multi-pack-index | missing |
| 19 | odb | Loose object size/trailer never validated | robustness |
| 20 | odb | Abbreviations don't search loose objects | known gap |
| 21 | refs | Loose ref parsing much stricter than git's | robustness |
| 22 | refs | No reftable backend | missing |
| 23 | commit-graph | No split graphs, no `GDA2`/`GDO2` | missing |
| 24 | repo | `.git/config` never read (SHA-256, extensions) | missing |
| 25 | repo | No `refs/replace`, grafts, or shallow | missing |
| 26 | integrity | Pack, index and commit-graph checksums unverified | design |
| 29 | archive | Long paths use GNU records, git uses pax | fidelity |
| 30 | archive | No `export-subst`, and no working-tree conversion | missing |

---

## Objects (`gib-object`)

### 1. A truncated header block panics **[probed]**

`continued_line` scans for a `\n` not followed by a space, and when it finds
none sets `slice_pos = input.len()` and then returns `&input[slice_pos + 1..]`
— a slice range one past the end (`crates/gib-object/src/header.rs:106-115`).

Two inputs that reach it:

```
Object::from_raw(id, RawObject { object_type: Commit, body: b"tree abc".to_vec() })
    → panicked at header.rs:114: range start index 4 out of range for slice of length 3

body = b"tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb\n"   // headers, no blank line
    → panicked at header.rs:114: range start index 42 out of range for slice of length 41
```

git returns `error("bogus commit object %s")` (`commit.c`, `parse_commit_buffer`)
and `-1` from `parse_tag_buffer` (`tag.c`). A panic in wasm aborts the whole
page, so a single truncated or half-written object takes the app down rather
than showing one broken commit. This is the one item here that is unambiguously
a crash bug.

### 2. Broken idents reject the whole object **[probed]**

`parse_author_committer_tagger` (`crates/gib-object/src/lib.rs:295-314`) demands
exactly `<name> SP '<' <email> '>' SP <seconds> SP [+-]HHMM`, all-consuming.
git's `split_ident_line` (`ident.c:275`) is far looser: it finds the first `<`,
walks *back* over whitespace for the name end, finds the closing `>` by scanning
from the end of the line, and treats the date and timezone as optional
(`goto person_only`). The timezone is `strspn(cp + 1, "0123456789")` — any
number of digits.

Every one of these parses in git and fails in gib, taking the whole commit with
it:

| Header value | gib |
|---|---|
| `a<e> 1 +0000` (no space before `<`) | `Parse` error |
| `a <e> 1` (no timezone) | `Parse` error |
| `a <e> 1 +051800` (six-digit tz) | `Parse` error |
| `a <e>` (no date at all) | `Parse` error |

Six-digit timezones and missing dates are not hypothetical — they occur in
histories imported from CVS/SVN and in the early git and kernel histories. A
repository containing one has an unviewable commit page (and an unviewable log,
since the walk fetches the object).

### 3. `author`/`committer` are mandatory **[probed]**

A commit missing either header is `ObjectError::MissingFields`
(`crates/gib-object/src/commit.rs:122-182`). `parse_commit_buffer` never looks
at those lines at all — it validates only that `tree ` heads the buffer and that
the `parent ` lines that follow are well-formed — so `git log` renders such a
commit with an empty author. Same for a tag with no `tagger`, which gib already
allows, and for `tag`/`type`, which it does not.

### 4. An unrecognised mode rejects the whole tree **[probed]**

`entry_type_from_mode` (`crates/gib-object/src/tree.rs:83-101`) returns `None`
for any mode whose `S_IFMT` bits aren't one of the four it knows, and because
the tree is parsed `all_consuming`, one bad entry loses every entry.

git never fails here. `parse_mode` (`object.h:200`) accepts any run of octal
digits (accumulating into a `uint16_t`, so it also wraps rather than
overflowing), and `canon_mode` (`object.h:145`) maps anything that is not
`S_IFREG`/`S_IFLNK`/`S_IFDIR` to `S_IFGITLINK`. So git reads mode `70000` as a
submodule entry; gib reports the tree as corrupt and the tree page shows
nothing.

The existing `parse_tree_rejects_nonsense_modes` test asserts the current
behaviour deliberately, so this is a decision to revisit rather than an
oversight — but the blast radius (whole tree, and every diff through it) argues
for git's canonicalisation.

### 5. Empty filenames are accepted **[probed]**

`take_till(|c| c == b'\0')` (`crates/gib-object/src/tree.rs:113`) matches zero
bytes, so `100644 \0<oid>` parses. `decode_tree_entry` rejects it:
`"empty filename in tree entry"` (`tree-walk.c:35`). Leniency in the direction
of accepting what git rejects, so lower priority — but it produces a tree row
with no name.

### Header ordering (informational)

git requires `tree` first and `parent` lines immediately after it
(`parse_commit_buffer`), and `object`/`type`/`tag` in exactly that order
(`tag.c`, `parse_tag_buffer`). gib matches headers by name in any order. Again
leniency, and arguably the better behaviour for a viewer.

---

## Line diff and patch (`gib-patch`)

The crate's doc comment already names three deliberate gaps (fixed seven-digit
abbreviations, no rename detection, binary files not encoded). These are the
undocumented ones.

### 8. Non-UTF-8 bytes become U+FFFD **[probed]**

`String::from_utf8_lossy` (`crates/gib-patch/src/diff.rs:342`) is irreversible.
For a Latin-1 file:

```
git line bytes: [45, 99, 97, 102, 233]                 // -caf\xe9
gib line bytes: [45, 99, 97, 102, 239, 191, 189]       // -caf\u{FFFD}
```

`PatchLine.text` is a `String`, so this is structural: fixing it means the type
becomes `Vec<u8>` (or the renderer keeps bytes and only the DOM layer goes
lossy). git writes the raw bytes through.

### 10. The indent heuristic is not switched on

`diff.indentHeuristic` defaults to on (`diff.c:57`), and git passes
`XDF_INDENT_HEURISTIC` in the `xpparam_t` flag word for every diff it prints.
`gib-xdiff`'s `unified` contributes only `Whitespace::flags`
(`crates/gib-xdiff/src/lib.rs:125-131`), which is either nothing or
`XDF_IGNORE_WHITESPACE` — so the heuristic bit is never set.

Since the diff is git's own xdiff now, everything else about hunk placement
matches: `xdl_change_compact` runs in both directions inside `xdl_diff` either
way. What is left is the shift the heuristic applies afterwards, which moves a
hunk boundary when several placements are equally valid — the case of an added
block that repeats lines already present. The earlier probe of this was against
the `similar`-based implementation and no longer describes the output, so a
fresh one is needed before the size of the gap is worth quoting.

### 11. No gitattributes

git consults `.gitattributes` for `diff` / `-diff` / `binary` / `text` before
diffing (`diff.c`, `diff_filespec_is_binary` and friends), and also treats a
file over `core.bigFileThreshold` as binary. gib's `is_binary`
(`crates/gib-patch/src/diff.rs:124`) is only git's `buffer_is_binary`
(`xdiff-interface.c:197-201`) — first 8000 bytes, NUL scan — which it matches
exactly. So a repository that marks a file `-diff` still gets a full line diff
here, and one that marks a NUL-free file `binary` still gets diffed.

`gib-attributes` (written for the archive walk, and differential-tested against
`git check-attr`) already parses and matches `.gitattributes`, so what is left
is consulting it from the diff path rather than new parsing.

---

## Packfiles (`gib-pack`)

### 12. Only pack v2 and index v2

* `validate_packfile_version` (`crates/gib-pack/src/pack.rs:136`) accepts only
  `PACK\0\0\0\2`. git accepts versions 2 **and 3**
  (`pack.h:17`, `pack_version_ok_native`).
* `FanoutTable::load` (`crates/gib-pack/src/index.rs:20-27`) requires the v2 magic.
  git also reads **version 1** indexes — a `.idx` with no magic at all is
  treated as v1 (`packfile.c:110-119`), with 24-byte `(offset, oid)` entries and
  no CRC or long-offset tables. Old mirrors still carry them.

Neither is validated the way git validates: git checks the fanout is monotonic
(`"non-monotonic index"`) and that the file size matches the entry count
exactly (`"wrong index v2 file size"`, `packfile.c:120-180`). gib checks
neither. It does reject a short read of the two tables it loads up front —
`FanoutTable::load` (`index.rs:22-25`) and `ShortOffsetTable::load`
(`index.rs:64-69`) both return `CorruptIndexFile` when `read_segment` comes back
short — but the ID-table binary search does not: `find_object_idx`
(`index.rs:109`) and `get_obj_packfile_offset` (`index.rs:205`, `:217`) ignore
the byte count, so an index truncated past the fanout binary-searches over
zero-filled buffers.

### 13. Corrupt deltas panic

`reconstruct_deltified_object` (`crates/gib-pack/src/pack.rs:277-327`) indexes
`base[offset..offset + size]` with values read straight out of the delta stream.
git bounds-checks all of it (`patch-delta.c:59-61`):

```c
if (unsigned_add_overflows(cp_off, cp_size) ||
    cp_off + cp_size > src_size || cp_size > size)
        goto bad_length;
```

and returns `NULL`. It also verifies the delta header's base size against the
actual base (`patch-delta.c:30-31`) and rejects a delta shorter than
`DELTA_SIZE_MIN`; gib's equivalents are `debug_assert_eq!`
(`pack.rs:281`, `pack.rs:320`), i.e. absent in release builds — so a
release-mode wasm build produces a wrong object where debug panics. That object
no longer reaches the page: it fails the id check described in item 26. The
panic and the missing bounds are still worth fixing — a rejected object is an
unviewable commit, and the debug-build panic is unchanged.

### 14. Delta opcode 0 is silently ignored

git treats `cmd == 0` as reserved and fails: `error("unexpected delta opcode 0")`
(`patch-delta.c:78`). gib's `instruction & 0b1000_0000 == 0` branch
(`pack.rs:289`) reads it as a zero-length append and continues, so a delta
stream with a reserved opcode produces a wrong object instead of an error —
caught downstream by the id check (item 26), but as a hash mismatch rather than
as the specific error git gives.

### 15. No bound on the offset-delta base

`form_deltified_chain` does `offset.0 -= base_offset_neg.0` (`pack.rs:253`) with
no check. git requires the base to be strictly earlier in the pack and non-zero
(`packfile.c:1032`):

```c
if (base_offset <= 0 || base_offset >= delta_obj_offset)
        return 0;  /* out of bound */
```

That invariant is also what guarantees termination. Without it, a
`base_offset_neg` of 0 is an infinite loop, and one larger than the current
offset underflows — panic in debug, wrap-around into a garbage read in release.
git additionally requires the computed base to land on a real object boundary
(`offset_to_pack_pos`, `packfile.c:1071`).

### 16. Truncated object headers panic

`read_obj_type_size`, `read_delta_offset` and `read_delta_expected_size` each
end in `assert!(done_accumulating_size, "buffer was too short to hold varsize")`
(`pack.rs:77`, `:103`, `:127`). git's `unpack_object_header_buffer`
(`packfile.c:869`) returns `used = 0` after `error("bad object header")`, and
explicitly guards the shift against overflow:

```c
if (len <= used || (bitsizeof(size_t) - 7) < shift) { ... }
```

gib's `obj_size.0 += u64::from(size_bits) << shift` has no shift guard either.

### Not implemented at all

Reachability bitmaps (`.bitmap`), pack reverse indexes (`.rev`), cruft packs,
and promisor/partial-clone packs. All are performance or partial-clone
features, none affect correctness for a fully-packed repository, and none are
reachable over dumb HTTP anyway.

---

## Object database (`gib-odb`)

### 17. No `objects/info/alternates`

git resolves alternates recursively at ODB startup (`odb.c:102`,
`parse_alternates`; `odb.c:487`, `odb_prepare_alternates`). `ObjectDb::open`
(`crates/gib-odb/src/lib.rs`) looks only at the one `objects/` directory it was
handed. Forge deployments that share object storage between forks — which is
the normal Forgejo/GitLab layout — will have objects that gib reports as
missing.

### 18. No multi-pack-index

git reads `objects/pack/multi-pack-index` (`midx.c`) before falling back to
individual `.idx` files. gib always searches every pack index in turn
(`ObjectDb::find_packed_object`). Correctness is unaffected as long as the
`.idx` files are still present, which `git multi-pack-index write` leaves them —
but `git multi-pack-index expire` can remove them, at which point objects
disappear.

### 19. Loose objects are never size-checked

`read_loose_object` (`crates/gib-odb/src/loose.rs:44-52`) inflates the whole
file and takes everything after the NUL as the body, ignoring the size the
header declares. git compares them and reports
`"corrupt loose object '%s'"` if the inflated length disagrees, and
`"garbage at end of loose object '%s'"` if the zlib stream has trailing input
(`object-file.c:245-251`). A truncated download therefore renders here as a
truncated file rather than as an error — though the id check (item 26) now
catches it a layer up, so what surfaces is a hash mismatch rather than a short
file.

git also rejects unknown loose object types up front (`parse_loose_header`);
gib's `parse_header` (`crates/gib-object/src/lib.rs:277`) does too, so that part
matches.

### 20. Abbreviations don't search loose objects

`resolve_prefix` walks pack indexes only, which
`crates/gib-odb/src/lib.rs:216-223` documents and justifies (no directory
listing over dumb HTTP). git's `find_short_object_filename` also scans
`objects/??/`. Listed for completeness — the reasoning is sound for this
transport.

---

## Refs (`gib-ref`, `crates/gib/src/reference.rs`)

### 21. Loose ref parsing is stricter than git's

`RefTarget::parse_loose_ref` (`crates/gib-ref/src/lib.rs:123-135`) requires the
content to be exactly one line **terminated by `\n`**, and a symbolic ref to
begin with the literal `ref: refs/`.

`parse_loose_ref_contents` (`refs/files-backend.c:669`):

```c
if (skip_prefix(buf, "ref:", &buf)) {
        while (isspace(*buf)) buf++;
        ...
```

so git accepts `ref:refs/heads/x`, `ref:\t refs/heads/x`, and a target that does
not live under `refs/` at all (`ref: HEAD` is legal, and reftable repositories
ship `.git/HEAD` containing `ref: refs/heads/.invalid`, `refs.c:2208`). git also
right-trims the file before parsing, so a missing trailing newline is fine, and
tolerates trailing data after an object id — that is how `FETCH_HEAD` works.

Each of these is `Error::MalformedRef` here, which for `HEAD` means the whole
repository fails to open.

Two consequences of the internal representation, worth knowing rather than
fixing: `RefName::Ref` stores names with `refs/` **stripped**, and
`parse_packed_refs` / `parse_info_refs` accept comment lines anywhere rather
than only as a leading header (git's `packed-refs` header must be the first
line).

### 22. No reftable backend

git 2.45+ supports `extensions.refStorage = reftable`
(`refs/reftable-backend.c`, `reftable/`), where there are no loose ref files and
no `packed-refs` — everything lives in `.git/reftable/`. Such a repository has a
`.git/HEAD` stub reading `ref: refs/heads/.invalid`, so `resolve_git_dir`
succeeds (it only probes for HEAD's existence,
`crates/gib/src/repo.rs:86-98`) and then every ref lookup finds nothing. The
failure mode is "empty repository", not a clear error.

`info/refs` from `update-server-info` does cover reftable repos, so a mirror
prepared for dumb HTTP still works — but a bare rsync of a reftable repo does
not.

---

## Commit-graph (`gib-commitgraph`)

The crate documents single-file-only support and degrading to `None`, which is
the right shape. What's missing beyond that:

### 23. Split graphs and generation-number chunks

* Chained graphs (`objects/info/commit-graphs/graph-*.graph` plus
  `commit-graph-chain`, chunk `BASE` = `0x42415345`) return `Ok(None)`
  (`crates/gib-commitgraph/src/lib.rs:141`). `git commit-graph write --split` is
  the incremental-write path, so a repository maintained that way loses the
  cache entirely and falls back to object reads.
* `GDA2` (`0x47444132`) and `GDO2` (`0x47444f32`) — corrected commit dates, i.e.
  generation numbers v2 (`commit-graph.c:48-49`) — are ignored. gib only reads
  the 34-bit commit time out of the `CDAT` trailer
  (`crates/gib-commitgraph/src/lib.rs:338-342`), which is correct as far as it
  goes; it simply means there is no generation number available for reachability
  or topological ordering, so a `--topo-order` equivalent is out of reach.

### Validation

git checks the graph version and hash version against the repository's hash algo
(`commit-graph.c:397-408`); gib hardcodes `header[4] == 1 && header[5] == 1`
(`lib.rs:133`), which is the same check for a SHA-1 repository. Neither verifies
the trailing checksum on the normal read path (git does under
`commit-graph verify`). gib does not bounds-check `BIDX` values against the
`BDAT` chunk length, nor `EDGE` indices against the chunk — a corrupt graph can
read arbitrary in-file bytes as a Bloom filter. All reads are still bounded by
the file, so this is a wrong-answer risk, not a memory-safety one.

The changed-path Bloom implementation itself (`bloom.rs`) matches `bloom.c`,
including the v1 sign-extended murmur3, and is covered by a no-false-negatives
differential test.

---

## Repository-level configuration (`gib`)

### 24. `.git/config` is never read

`Repo::open_with_config` (`crates/gib/src/repo.rs:64-79`) opens `objects/`, the
commit-graph, and nothing else. Consequences:

* **SHA-256 repositories** (`extensions.objectFormat = sha256`,
  `hash.h:191`) are undetectable. `ObjectId` is a hard `[u8; 20]`
  (`crates/gib-hash/src/lib.rs:19`) and `OID_LEN`/`take(20usize)` are baked into
  the pack, tree and commit-graph readers, so this is a deep change, not a
  config read. The crate documents SHA-1-only, so this is a scope statement — it
  just isn't *detected*, and a SHA-256 repo will misparse rather than refuse.
* `core.repositoryFormatVersion` and unknown `extensions.*` are not checked, so
  gib will happily half-read a repository git itself would refuse to open.
* `core.abbrev` is ignored; `gib-patch` hardcodes 7 digits
  (documented), where git widens abbreviations until unambiguous.

### 25. No `refs/replace`, grafts, or shallow

* git substitutes objects named by `refs/replace/*` on every lookup
  (`lookup_replace_object`) unless `GIT_NO_REPLACE_OBJECTS` is set. gib returns
  the original object, so a repository using replace refs to graft history shows
  the pre-graft history.
* `info/grafts` and `shallow` are likewise unread (`parse_commit_buffer` honours
  grafts inline; `commit.c`). On a shallow mirror the walk will chase a parent
  that isn't there and surface `MissingObject` instead of stopping at the
  boundary.

### 26. Only object bodies are verified

Object bytes are now hashed on arrival. `CachingRepo::lookup_raw`
(`src/cache.rs:124`) calls `RawObject::verify`
(`crates/gib-object/src/lib.rs:78-107`) against the id the object was fetched
under, before the bytes are parsed and before they reach IndexedDB. Every path
an object can arrive by passes through that one boundary — loose, packed, and
rebuilt from a delta chain — so all three are covered, and the hash itself is
pinned to `git hash-object` vectors (`compute_id_matches_git`) and exercised
over a `gc --aggressive` pack (`packed_objects_hash_to_their_ids`). git does the
same when parsing: `parse_object` calls `check_object_signature` unless
`PARSE_OBJECT_SKIP_HASH_CHECK` is set (`object.c:380-381`), and
`stream_object_signature` for large blobs.

Still unchecked:

* the `.idx`/`.pack` trailing checksums and the per-object CRC32 in an index v2;
* the commit-graph trailer;
* anything read through `gib` directly — the odb hands back whatever it
  reconstructed, so the check is the application's, not the library's.

`ARCHITECTURE.md` says the repository is assumed non-malicious, so what is left
is a stated design position rather than an oversight. The position is now
narrower: a substituted or corrupted object *body* is caught wherever it came
from, while a substituted pack index or commit-graph is not — a bad `.idx` can
still send a lookup to the wrong offset, where it surfaces as a hash mismatch
rather than as the wrong object.

---

## Archive (`gib-archive`)

The tar writer is differential-tested against `git archive --format=tar` and
matches on mode normalisation (`tar.umask` 002 → 0664/0775/0777), the
`pax_global_header` carrying the commit id, `root`/`root` ownership, and
whole-archive padding to 10 KiB. Two gaps:

### 29. Over-long paths use GNU records, not pax

git emits pax extended headers for a path or linkpath that doesn't fit ustar's
100-byte name + 155-byte prefix split, and for a file over 8 GiB
(`archive-tar.c:291`, `:301`, `:308-312`). The `tar` crate emits GNU
`LongName`/`LongLink` records instead — this is even called out in the comment
at `crates/gib-archive/src/writer.rs:173`. Both are readable by GNU tar, but the
bytes are not git's, so the "interchangeable with git's" claim in the module doc
holds only for paths under the ustar limit.

### 30. No `export-subst`, and no working-tree conversion

`export-ignore` is now read (`gib-attributes`, differential-tested against
`git check-attr` and `git archive`), but it is the only attribute the walk
looks at. Two others still change what git writes:

* **`export-subst`** expands `$Format:...$` placeholders in a file's content
  against the commit being archived (`archive.c:180` sets the flag,
  `:108-109` does the expansion). A file carrying one is archived here with the
  placeholder still in it.
* **The working-tree conversion** — `text`/`eol`, `working-tree-encoding`, and
  clean/smudge filters — is run over every regular file on its way into the
  archive (`archive.c:107`), not only over files marked `export-subst`. gib
  writes the blob's bytes as they are stored, so a repository that asks for
  CRLF endings in its tarball doesn't get them.

---

## Things checked and found to match

Worth recording so they aren't re-litigated:

* Binary detection: NUL in the first 8000 bytes, exactly `buffer_is_binary`
  (`xdiff-interface.c:197`).
* Everything the line diff itself decides, since `gib-xdiff` links git's own
  `xdiff/`: hunk grouping, the `@@` headers' enclosing-function suffix
  (`XDL_EMIT_FUNCNAMES`), CRLF and bare-CR handling (lines are split on `\n`
  alone and their bytes are written through), and `-w` via
  `XDF_IGNORE_WHITESPACE`. Items 6, 7 and 9 were all consequences of the
  `similar`-based diff this replaced.
* Default diff context of 3 lines, and `\ No newline at end of file`
  placement.
* Tree entry ordering in the diff walk — sorting a directory as if its name
  ended in `/` (`crates/gib-diff/src/lib.rs:110-135`) matches git's
  `base_name_compare`, and is differential-tested.
* Executable-bit and symlink mode canonicalisation for the four modes git
  actually writes, matching `canon_mode`.
* Packed object type codes, the three packfile varint encodings, and the
  offset/size delta instruction layout.
* Commit-graph `CDAT` layout, the `EDGE` octopus encoding, and the
  30-bit-generation / 34-bit-time split.
* Changed-path Bloom filters, including v1 sign-extended murmur3 and the two
  seeds.
* `objects/info/packs` and `info/refs` formats.
* Submodules in an archive: a gitlink is written as an empty directory rather
  than skipped, and its attributes are looked up with the trailing slash a real
  directory gets — so a `dir/`-only `export-ignore` pattern drops one.
  Differential-tested in `gib-archive`.
* Symref depth limit of 5, matching `SYMREF_MAXDEPTH`.
* Pack-first, loose-second lookup order, matching
  `do_oid_object_info_extended`.
* `git format-patch`'s mbox header, RFC 2047 encoding, subject folding, and the
  diffstat column arithmetic from `show_stats`.
* The history walk's frontier ordering (newest commit time first, ties broken by
  discovery order, matching `prio-queue.c`'s insertion-counter tie-break) and
  its default path simplification — both the TREESAME test and the parent
  rewriting a merge triggers (`try_to_simplify_commit`), including consulting a
  changed-path Bloom filter for the first parent only. Differential-tested in
  `gib-log` against `git rev-list`, with and without a commit-graph.

---

## Suggested order of work

1. **Item 1** — the header-block panic. A crash from one malformed object,
   trivially fixed by returning a parse error when no terminator is found.
2. **Items 13, 15, 16** — the pack panics and the missing delta bounds. Same
   shape of problem, same fix: turn `assert!`/slice indexing into errors, and
   add git's `base_offset < obj_offset` invariant.
3. **Item 2** — ident leniency, which is what makes real imported histories
   unviewable.
4. **Item 10** — one flag bit, once a probe says what it changes.
5. **Item 8** — needs byte-level `PatchLine`s, so it is a type change through
   the renderer rather than a local fix.
6. **Items 17, 22** — alternates and reftable, if deployment targets need them.
