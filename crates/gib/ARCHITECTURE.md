The user-facing crate tying everything together: `Repo` composes an
`ObjectDb`, an optional commit-graph, and ref resolution (loose, packed, and
`info/refs`), and the prelude's `ObjectExt`/`RefExt` traits add peeling on
top. It re-exports the sub-crates under stable module paths so consumers
(webgit) depend on this crate alone. Repository-level policy and
crate-spanning integration tests live here.
