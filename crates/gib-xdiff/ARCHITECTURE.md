Line-level diffing of two blobs, by compiling git's own xdiff rather than
reimplementing it. `vendor/xdiff` is the libgit2 extraction of the library git
uses, vendored as a submodule and built unmodified; this crate is the shim that
lets it run without a libc plus a safe Rust surface over it.
