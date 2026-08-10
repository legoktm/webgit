A cgit-inspired repository viewer that runs entirely client-side on top of 
100% static hosting. The eventual goal is to provide all of cgit's featureset.

## Backend

The only requirement is that the git repository has run `git update-server-info`;
we recommend and optionally use the commit-graph if it's present.

The frontend fetches objects over Git's "dumb HTTP" protocol using `Range` requests
and stores them in the browser's IndexedDB.

### Sub-crates

The git logic itself is stored in `gib-` crates, which enables verification testing against the CLI `git`.
The layout and split is inspired by gitoxide's subcrates, but the mapping isn't the same.

Each crate has its own ARCHITECTURE.md that outlines the rough scope of the crate, but the boundaries are
flexible if it would be useful.

## UI

The UI is largely a copy of cgit, keeping the minimalist, monospace look, just with an added blue tint.

## Performance

We optimize for minimizing the amount of git data that needs to be loaded/fetched over HTTP; if it's
already in IndexedDB then it's essentially free.

Where possible, we lazily render data instead of requiring every lookup to be complete.

## Security

We set as strict a CSP as possible, banning inline CSS and JS.

We mostly assume that the Git repository is not malicious. But we still take some precautions where
we can, such as rendering markdown in an iframe.
