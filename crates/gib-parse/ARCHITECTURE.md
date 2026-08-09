Shared low-level parsing support used by every format-parsing crate: the nom
`ParseError`/`ParseResult` glue that truncates failing input into error
snippets, and the `SubsliceRange` helper for storing stable ranges into a
parsed buffer. It contains no git knowledge of its own — it exists so
`gib-hash`, `gib-object`, and `gib-ref` can share one parsing-error
vocabulary. Pure and synchronous; no IO.
