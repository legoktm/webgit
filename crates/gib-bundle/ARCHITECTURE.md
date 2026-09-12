Reading and writing a `git bundle` — the single file `git clone` can clone from.
Writing is the walk that enumerates every object a set of refs reaches and the
writer that turns those objects into the bundle's header and packfile; objects
are deltified as git deltifies them, through `gib-diff-delta` and a port of
`pack-objects.c`'s window heuristics, because an undeltified bundle is around
four times the size of git's. Reading is the reverse over a bundle someone else
wrote: the header's refs, then a single pass over the packfile rebuilding every
object — including git's thin-pack deltas against objects the bundle doesn't
carry — and naming each one by the hash of what came out rather than by anything
the file claims. Does no IO and never holds a repository: objects arrive through
the caller's `ObjectSource` and leave as bytes a piece at a time, or arrive as a
pack and leave through the caller's `ObjectStore`.
