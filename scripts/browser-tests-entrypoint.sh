#!/usr/bin/env bash
# Runs inside the container built from the repository's Containerfile. Not
# meant to be run on a host directly — use scripts/browser-tests.sh, which
# builds the image and mounts everything this expects.
#
# Any arguments are passed through to the test binary, so a single test can be
# selected:  scripts/browser-tests.sh core_routes
set -euo pipefail

profile="${WEBGIT_BROWSER_TESTS_PROFILE:-release}"
dist="${WEBGIT_DIST:-/cache/dist}"

build_args=(--dist "$dist")
case "$profile" in
    release) build_args+=(--release) ;;
    debug)   ;;
    *)
        echo "WEBGIT_BROWSER_TESTS_PROFILE must be 'release' or 'debug', got '$profile'" >&2
        exit 2
        ;;
esac

echo "==> trunk build ($profile) -> $dist"
trunk build "${build_args[@]}"

# --locked: the source is mounted read-only, so a lockfile update would fail
# with a confusing permissions error rather than an honest one.
#
# --test-threads=1: geckodriver serves a single session at a time.
echo "==> browser tests"
exec cargo test --locked -p browser-tests --features browser -- --test-threads=1 "$@"
