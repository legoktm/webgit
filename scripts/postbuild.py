"""Trunk post-build hook.

Three steps, run in order:

1. Relocate the hashed CSS/JS/WASM that Trunk emits at the dist root into an
   `assets/` subdir, matching the `/assets/` prefix from `public_url`.
2. Replace Trunk's inline module loader with an external, self-initializing
   module script. That removes the only inline <script> on the page, which lets
   the Content-Security-Policy drop 'unsafe-inline' from script-src.
3. Turn markdown.css's <link> into a <meta>, so the page doesn't apply a
   stylesheet meant only for the sandboxed readme frame while the app can still
   read its hashed URL at startup (see `assets::init`).
"""

import base64
import hashlib
import os
import re
import shutil
import sys
from pathlib import Path

dist = Path(os.environ["TRUNK_STAGING_DIR"])

# 1. Relocate every top-level build artifact into assets/ (keep index.html).
assets = dist / "assets"
assets.mkdir(exist_ok=True)
for entry in dist.iterdir():
    if entry.is_file() and entry.name != "index.html":
        shutil.move(str(entry), str(assets / entry.name))

# 2. Find the wasm-bindgen glue module (the --target web entrypoint).
glue = next(
    (p for p in assets.glob("webgit-*.js") if not p.name.endswith("_bg.js")),
    None,
)
if glue is None:
    sys.exit("postbuild: glue JS not found")

# Make the module boot itself when loaded as <script type="module" src=...>.
# The Rust entrypoint is #[wasm_bindgen(start)], so __wbg_init() alone runs the
# app; we only have to point it at the hashed wasm next to this module (the
# generated default path uses the un-hashed name and would 404).
with glue.open("a") as fh:
    fh.write(
        "\n// webgit: self-initialize, replacing Trunk's inline module loader.\n"
        "__wbg_init({ module_or_path: "
        'new URL(import.meta.url.replace(/\\.js$/, "_bg.wasm")) });\n'
    )

# Recompute the subresource-integrity hash now that the file changed.
digest = hashlib.sha384(glue.read_bytes()).digest()
integrity = "sha384-" + base64.b64encode(digest).decode()

index = dist / "index.html"
html = index.read_text()

# Refresh the modulepreload's integrity (scoped to that tag so the adjacent
# wasm preload's integrity is left intact).
html, n = re.subn(
    r'(<link rel="modulepreload"[^>]*integrity=")sha384-[^"]*',
    lambda m: m.group(1) + integrity,
    html,
)
if n != 1:
    sys.exit(f"postbuild: expected 1 modulepreload integrity, patched {n}")

# Swap Trunk's inline loader for an executing external module script.
external = (
    f'<script type="module" src="/assets/{glue.name}" '
    f'crossorigin="anonymous" integrity="{integrity}"></script>'
)
html, n = re.subn(
    r'<script type="module">.*?</script>', external, html, flags=re.DOTALL
)
if n != 1:
    sys.exit(f"postbuild: expected 1 inline module script, replaced {n}")

# 3. Demote markdown.css from a stylesheet to a meta tag. It styles only the
# readme frame's document, so applying it here would be wrong (and would fetch
# it on every page load); a meta keeps the hashed URL readable from JS/wasm
# without costing a request. Integrity is dropped with the <link> — the frame
# loads it as an ordinary same-origin stylesheet.
html, n = re.subn(
    r'<link rel="stylesheet" href="(/assets/markdown-[^"]*\.css)"[^>]*/?>',
    lambda m: f'<meta name="markdown-css" content="{m.group(1)}">',
    html,
)
if n != 1:
    sys.exit(f"postbuild: expected 1 markdown.css link, rewrote {n}")

index.write_text(html)
