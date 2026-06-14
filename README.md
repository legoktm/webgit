webgit is a fully client-side Git repository viewer. At a very high level, it
acts like a Git client, fetching and storing individual Git objects in IndexedDB,
and then walking the tree to calculate and display files, diffs, logs, etc.

This is largely a proof-of-concept; it should work, but it hasn't really been extensively
tested for all the different edge cases for things you can do with Git. If you want to deploy
it for your own purposes, that would be sick, but it may not be super mature.

webgit vendors a fork of <https://github.com/cyberia-ng/git-async> in order
to enable storage of objects in IndexedDB.

For the most part webgit is aiming to imitate cgit (<https://git.zx2c4.com/cgit/about/>)
as closely as possible because it's probably my favorite Git viewer.

In as much as there is copyrightable code in this repository, it's available under the GPL v2
as a derivative work of Git and/or cgit.

### Set up

* Install Rust for the `wasm32-unknown-unknown` target (e.g. `rustup target add wasm32-unknown-unknown`).
* Install [`trunk`](https://trunk-rs.github.io/trunk/guide/getting-started/installation.html).
* Clone this repository and run `trunk build --release`.
* Copy the `dist/` directory to your web server, e.g. `/var/www/webgit/`.
* You'll then need to set up your webserver to serve `index.html` in place of each repository. For Apache,
  my configuration looks like:

```apache
Alias /assets     /var/www/webgit/dist/assets
<Directory /var/www/webgit/dist/>
    Require all granted
</Directory>

# Rewrite /public/foo.git/ + /mirrors/foo.git/ to the webgit index
RewriteCond %{REQUEST_URI} ^/(public|mirrors)/[^/]+\.git/$
RewriteRule ^ /var/www/webgit/dist/index.html [L]
```

### Repositories

Your webserver should be set up to serve bare Git repositories. It's strongly recommended that you
run `git update-server-info` in each.

I merely rsync the repositories from Forgejo's storage to my webserver and it seems to work fine.

### Index listing

You'll need to create a `/listing.json` file that contains a JSON array of paths to repositories.

Then you can serve the same dist/index.html under your webroot and it will automatically render
the listing instead.
