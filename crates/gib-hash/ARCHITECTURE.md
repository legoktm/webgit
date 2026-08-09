The object-ID vocabulary of the whole library: `ObjectId` (SHA-1, 20 bytes),
abbreviated `ObjectIdPrefix` with its range and comparison logic, and
`PrefixResolution` for combining prefix-search results across multiple
sources. Every downstream crate — objects, refs, packs, the odb — speaks in
these types. Pure data plus hex parsing; no IO.
