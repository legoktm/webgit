Building a `git bundle` — the single file `git clone` can clone from — for a set
of refs: the walk that enumerates every object they reach, and the writer that
turns those objects into the bundle's header and packfile. Objects are deltified
as git deltifies them, through `gib-diff-delta` and a port of `pack-objects.c`'s
window heuristics, because an undeltified bundle is around four times the size of
git's. Does no IO and never holds a repository: objects arrive through the
caller's `ObjectSource` and leave as bytes a piece at a time.
