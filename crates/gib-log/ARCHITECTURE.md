Walking a repository's history the way `git log` does: the commit frontier, the
pagination window, and the path filter that decides which commits a `log/?path=`
view is allowed to show.

The crate does no IO. Commits, trees and commit-graph records arrive through the
caller's `CommitSource`, so the same walk runs over a browser's cached object
store and over a plain on-disk one in the differential tests. It also renders
nothing: the walk hands back `Commit`s (streaming the page as it fills, so a
caller can paint rows before the last object lands) and a `WalkStats`, and the
caller turns those into whatever its UI wants.
