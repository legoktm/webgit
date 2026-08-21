"""Trunk post-build hook.

Four steps, run in order:

1. Relocate the hashed CSS/JS/WASM that Trunk emits at the dist root into an
   `assets/` subdir, matching the `/assets/` prefix from `public_url`.
2. Replace Trunk's inline module loader with an external, self-initializing
   module script. That removes the only inline <script> on the page, which lets
   the Content-Security-Policy drop 'unsafe-inline' from script-src.
3. Turn markdown.css's <link> into a <meta>, so the page doesn't apply a
   stylesheet meant only for the sandboxed readme frame while the app can still
   read its hashed URL at startup (see `assets::init`).
4. Write dist/.htaccess, carrying the Content-Security-Policy from
   webcat/webcat.config.json and an X-WEBCAT-Version header for this build.
"""

import json
import os
import re
import shutil
import subprocess
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

index = dist / "index.html"
html = index.read_text()

# Swap Trunk's inline loader for an executing external module script.
external = (
    f'<script type="module" src="/assets/{glue.name}" crossorigin="anonymous">'
    "</script>"
)
html, n = re.subn(
    r'<script type="module">.*?</script>', external, html, flags=re.DOTALL
)
if n != 1:
    sys.exit(f"postbuild: expected 1 inline module script, replaced {n}")

# 3. Demote markdown.css from a stylesheet to a meta tag. It styles only the
# readme frame's document, so applying it here would be wrong (and would fetch
# it on every page load); a meta keeps the hashed URL readable from JS/wasm
# without costing a request.
html, n = re.subn(
    r'<link rel="stylesheet" href="(/assets/markdown-[^"]*\.css)"[^>]*/?>',
    lambda m: f'<meta name="markdown-css" content="{m.group(1)}">',
    html,
)
if n != 1:
    sys.exit(f"postbuild: expected 1 markdown.css link, rewrote {n}")

index.write_text(html)

# 4. Emit the .htaccess that sends the response headers WEBCAT checks, so a
# deployment doesn't have to copy them into its webserver config by hand.
#
# The CSP is read from webcat/webcat.config.json, the same file the signed
# manifest is generated from: the extension compares the header against the
# manifest byte for byte, on every response from the domain, so the two must
# never drift. Deliberately not wrapped in <IfModule mod_headers.c> — without
# mod_headers the site would silently serve no CSP at all, and a 500 that says
# so is the better failure.
config = json.loads(Path("webcat/webcat.config.json").read_text())

# The workflow exports VERSION (the tag for a release, the commit otherwise)
# and stamps the same value into the manifest. Outside CI, fall back to the
# same `git describe` that `git_version!()` bakes into the wasm, and to the
# config's own version when there's no checkout to describe.
version = os.environ.get("VERSION")
if not version:
    described = subprocess.run(
        ["git", "describe", "--tags", "--always"], capture_output=True, text=True
    )
    version = described.stdout.strip() if described.returncode == 0 else config["version"]

if '"' in config["default_csp"] or '"' in version:
    sys.exit("postbuild: a header value contains a quote")

(dist / ".htaccess").write_text(
    f"""\
Header always set Content-Security-Policy "{config["default_csp"]}"
Header always set X-WEBCAT-Version "{version}"
"""
)
