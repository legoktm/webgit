Rendering a commit as a patch, in the shape `git format-patch` writes: the
line-level diff of one file at a time, and the assembly of the whole document
around it — mbox header, RFC 2047-encoded header fields, message, diffstat,
diff, signature. Diff lines are classified so a viewer can colour them without
re-parsing. Walking trees is `gib-diff`'s job and loading blobs is `gib-odb`'s;
the caller supplies each side's entry and bytes, which is what lets the browser
stream a diff in and still get an identical patch out.