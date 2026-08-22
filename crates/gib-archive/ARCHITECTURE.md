Building the contents of `git archive` for a tree: the walk that fetches every
object under it, and the tar writer that turns the result into the bytes git
itself would write — same mode normalisation, same `pax_global_header` carrying
the commit id, same whole-archive record padding, checked against a real
`git archive` invocation.

The crate does no IO: objects arrive through the caller's `ObjectSource`, which
is what lets the walk overlap its fetches without knowing where they go. Nor
does it compress — `TarWriter` hands out the tar in pieces so a caller can feed
them to whatever encoder it has (in webgit, the browser's own `CompressionStream`).
