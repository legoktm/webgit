Individual files copied from [git](https://github.com/git/git), each carrying a
`// Source: git.git:<path>@<commit>` line naming where it came from. They are
otherwise verbatim — whatever a file needs from its host is supplied by a shim
beside the crate that builds it — so refreshing one is a copy and a new
`Source:` line.

`diff-delta.c` is built by `crates/gib-diff-delta`.

git is GPL-2.0 **only**, so a binary built from this tree is distributable under
GPL-2.0 rather than the workspace's GPL-2.0-or-later.
