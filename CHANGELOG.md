## 0.2.0 / 2026-08-24

* Make README the default view; render markdown files
* Support downloading snapshots (tarballs)
* Support downloading patches of commits
* Rebrand as "gib", aka git-in-browser
* Add WEBCAT support and signed releases
* Display git notes
* Rewrite names and email addresses per `.mailmap`
* Support blame

Viewing individual commits/diffs:
* Don't display garbage diffs of binary files
* Link SHA1s in commit messages
* Correctly indicate file mode changes
* Fix showing diff for initial commit
* Add controls to adjust context or switch to a side-by-side diff

Viewing files:
* Render JPG, PNG, GIF images
* Optionally render SVGs and markdown files
* Add link to download a file

Viewing log:
* Allow expanding commit messages to see them in their entirety

Viewing trees:
* Better indicate submodules and symlinks
* Add "log" and "blame" links for each item

Repository index (landing page):
* Show tabs, allow access to "about" menu

Storage:
* Don't split IndexedDB storage by repository name; allows for serving multiple copies of a repository under different names

Internal/architecture:
* Use commit-graph if available for faster lookups
* Use yew to render everything instead of static templates; lazy-load in some places
* Add differential test suite to verify behavior against `git` CLI
* Implement browser testing
* Refactor into individual `gib-*` crates, after gitoxide
* Add overall ARCHITECTURE.md
* Implement SHA-1 integrity checks when writing objects
* Use cargo-cooldown, set to 7 days
* Use git's xdiff library for diffs, patches and blame instead of similar crate
* Pin to specific rust-toolchain.toml

## 0.1.0 / 2026-06-14

* Initial release
