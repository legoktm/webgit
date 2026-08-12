#!/usr/bin/env bash
# Build and run the browser test suite in a podman container.
#
#   scripts/browser-tests.sh                  # everything
#   scripts/browser-tests.sh core_routes      # one test, by name filter
#
# Environment:
#   WEBGIT_BROWSER_TESTS_PROFILE=debug  build dist/ without the release
#                                       profile's fat LTO, for faster iteration
#   WEBGIT_BROWSER_TESTS_IMAGE=<name>   override the image tag
#   NO_CACHE=1                          rebuild the image from scratch
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
image="${WEBGIT_BROWSER_TESTS_IMAGE:-webgit-browser-tests}"

build_args=()
[ "${NO_CACHE:-0}" = "1" ] && build_args+=(--no-cache)

# `git_version!()` in src/render/about.rs runs `git describe` during the wasm
# build, so the container needs a usable repository at /src.
#
# Two things get in the way. In a linked worktree, `.git` is a file pointing at
# a gitdir outside the mounted tree, so it has to be mounted too — at the same
# absolute path, since that is what the pointer says. And in any checkout, the
# files belong to the host user rather than the container's, which git rejects
# as dubious ownership unless told otherwise.
git_mounts=()
git_common_dir=$(cd "$repo_root" && git rev-parse --git-common-dir 2>/dev/null || true)
if [ -n "$git_common_dir" ]; then
    git_common_dir=$(cd "$repo_root" && cd "$git_common_dir" && pwd)
    case "$git_common_dir" in
        "$repo_root"/*) ;; # already inside the mount
        *) git_mounts+=(-v "$git_common_dir:$git_common_dir:ro,z") ;;
    esac
fi

echo "==> podman build $image"
podman build "${build_args[@]}" -t "$image" \
    -f "$repo_root/crates/browser-tests/Containerfile" "$repo_root"

# --shm-size: Firefox crashes in a container with podman's 64 MB default.
#
# The source is mounted read-only. Everything the run produces — the cargo
# target dir, the registry cache, and the trunk output — lands in named volumes
# instead, so a rootless run cannot leave files behind that the host user has no
# uid mapping for and therefore cannot delete.
#
# :ro,z relabels for SELinux, which is enforcing on the Fedora hosts this is
# developed on; without it the container is denied access outright.
echo "==> podman run"
exec podman run --rm \
    --shm-size=2g \
    -v "$repo_root:/src:ro,z" \
    "${git_mounts[@]}" \
    -v webgit-browser-tests-cache:/cache \
    -v webgit-browser-tests-cargo:/home/runner/.cargo/registry \
    -e "WEBGIT_BROWSER_TESTS_PROFILE=${WEBGIT_BROWSER_TESTS_PROFILE:-release}" \
    -e GIT_CONFIG_COUNT=1 \
    -e GIT_CONFIG_KEY_0=safe.directory \
    -e GIT_CONFIG_VALUE_0='*' \
    "$image" \
    /src/scripts/browser-tests-entrypoint.sh "$@"
