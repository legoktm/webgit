The object database: given a repository's `objects/` directory, `ObjectDb`
discovers packs, caches their indexes, and looks up objects — packed first,
loose as a fallback — and expands abbreviated IDs by searching every pack
index. It orchestrates `gib-pack` and the loose-object format and owns all
"where does an object live" policy; it does not parse object bodies (that is
`gib-object`) and knows nothing about refs or HEAD (that is the facade).
