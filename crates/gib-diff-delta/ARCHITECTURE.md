Binary delta encoding for packfiles, by compiling git's own `diff-delta.c`
(vendored in `vendor/git/`) rather than reimplementing it.

`shim/` supplies the handful of host facilities the file expects, so the
vendored copy stays verbatim.
