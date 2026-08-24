gib ("git-in-browser") is a fully client-side Git repository viewer. At a very high level, it
acts like a Git client, fetching and storing individual Git objects in IndexedDB,
and then walking the tree to calculate and display files, diffs, logs, etc.

This is largely a proof-of-concept; it should work, but it hasn't really been extensively
tested for all the different edge cases for things you can do with Git. If you want to deploy
it for your own purposes, that would be sick, but it may not be super mature.

For the most part webgit is aiming to imitate cgit (<https://git.zx2c4.com/cgit/about/>)
as closely as possible because it's probably my favorite Git viewer.

In as much as there is copyrightable code in this repository (most of it is vibecoded), it's available under the GPL v2
as a derivative work of Git and/or cgit. It vendors git's [xdiff](https://github.com/libgit2/xdiff) code, which is LGPL v2.1 or later. Some of the git handling code originated from <https://github.com/cyberia-ng/git-async>,
which is also available under the MIT or Apache 2.0 licenses.

### Set up

* Download the latest signed release from [GitHub](https://github.com/legoktm/webgit/releases).
* Copy the `dist/` directory to your web server, e.g. `/var/www/webgit/`.
* You'll then need to set up your webserver to serve `index.html` in place of each repository. For Apache,
  my configuration looks like:

```apache
Alias /assets     /var/www/webgit/dist/assets
<Directory /var/www/webgit/dist/>
    Require all granted
</Directory>

Header always set Content-Security-Policy "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src blob:; connect-src 'self'; base-uri 'none'; form-action 'none'"

# Rewrite /public/foo.git/ + /mirrors/foo.git/ to the webgit index
RewriteCond %{REQUEST_URI} ^/(public|mirrors)/[^/]+\.git/$
RewriteRule ^ /var/www/webgit/dist/index.html [L]
```

You should definitely enable HTTP/2 on your webserver too.

### Repositories

Your webserver should be set up to serve bare Git repositories. It's strongly recommended that you
run `git update-server-info` in each.

I rsync the repositories from Forgejo's storage to my webserver and it seems to work fine.

#### Commit Graph

To optimize a number of git operations, you should generate a "commit-graph" file
by running `git commit-graph write --reachable --changed-paths`.

### Index listing

You'll need to create a `/listing.json` file that contains a JSON array of objects, each mapping a
directory prefix to the repositories under it:

```json
[
    {"public": ["foo.git", "bar.git"]},
    {"mirrors": ["linux.git"]}
]
```

This will be transformed into the index.

If your repositories already sit in a directory tree, you can generate the file from it, using each
repository's parent directory as its prefix:

```sh
find /var/www/repos -type d -name '*.git' -prune -printf '%P\n' | sort |
    jq -R -s '[splits("\n")|select(. != "")]|map(split("/"))|group_by(.[:-1])|map({(.[0][:-1]|join("/")): map(.[-1])})' > /var/www/repos/listing.json
```

### WEBCAT

[WEBCAT](https://webcat.tech/) allows users to verify the website in question is running a signed,
non-tampered version of gib. WEBCAT is in alpha and support is still experimental.
