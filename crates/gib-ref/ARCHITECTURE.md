Parse-only types for git references: `RefName` and the parsers for `HEAD`,
loose ref file contents, `packed-refs`, and dumb-HTTP `info/refs`. Resolving
refs against a live repository — walking symrefs, loose-over-packed
precedence — is the facade's job; this crate never touches a filesystem.
