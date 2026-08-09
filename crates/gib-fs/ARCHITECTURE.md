The abstract filesystem boundary of the library: the
`FileSystem`/`Directory`/`File` traits that consumers implement (over
`std::fs` in tests, or HTTP + IndexedDB in webgit), plus `FileSystemError`,
`Offset`, and small read helpers. It is also home to `CachingPageReader`, a
4 KiB page cache over `File` used for lazy reads of pack indexes, packs, and
commit-graphs. This crate defines interfaces and generic IO utilities only;
it contains no git semantics.
