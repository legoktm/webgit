Tree-to-tree diffing: `TreeDiff` walks two trees through an `ObjectDb` and
reports added, deleted, and modified paths with the blob IDs on each side.
Line-level diffing of blob contents is deliberately out of scope and lives in
`gib-patch`. Checked against `git diff-tree --raw`.
