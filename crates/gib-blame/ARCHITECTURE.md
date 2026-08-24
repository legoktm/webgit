Line-by-line attribution: which commit last touched each line of one file, the
way `git blame` decides it.

The port is of git's `blame.c` as cgit drives it — `assign_blame(&sb, 0)`, so no
`-M`/`-C` move-and-copy detection, no `--ignore-rev`, no `--reverse`, and no
whole-file rename following. What is left is the scoreboard: a commit queue
ordered exactly as `gib-log`'s walk orders one, origins in an arena instead of
refcounted allocations, and `blame_chunk`'s splitting of line groups across the
hunks `gib-xdiff` reports. Those hunks come from git's own xdiff, which is the
whole reason the attribution can agree with git's line for line.

Like `gib-log`, the crate does no IO and renders nothing: objects arrive through
that crate's `CommitSource`, and the result is a list of line groups the caller
turns into whatever its UI wants. Checked against `git blame --line-porcelain`.
