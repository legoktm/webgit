Reader for git's optional `commit-graph` file (single-file form only), which
caches each commit's root tree, parents, and commit time — plus changed-path
Bloom filters — so history can be walked without inflating and parsing a
commit object per step. It reads lazily in 4 KiB pages via `gib-fs`, and any
missing, unsupported, or corrupt graph degrades to `None` so callers
transparently fall back to object reads.
