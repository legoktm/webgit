Everything about packfiles and their `.idx` indexes: fanout and offset table
lookups, object-header varints, delta-chain formation and reconstruction, and
streaming zlib inflation. It reads lazily through the `gib-fs` traits (paged
reads), so it performs IO, but it knows nothing about repository layout or
pack discovery — callers hand it already-open index and pack file handles.
Its output is verified against `git verify-pack`.
