To build this locally:

* Install [`rustup`](https://rustup.rs/).
* Install [`trunk`](https://trunk-rs.github.io/trunk/guide/getting-started/installation.html).
* Install `clang`, which builds the vendored C in `vendor/xdiff`.
* Clone this repository with `--recurse-submodules` (or run `git submodule
  update --init` in an existing clone).
* Run `trunk build [--release]`.

Issues may be filed on [GitHub](https://github.com/legoktm/webgit). At this time, external pull requests are not accepted.
