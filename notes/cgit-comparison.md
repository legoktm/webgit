# cgit features webgit doesn't support yet

A gap list against cgit, based on the routes in `src/route.rs` and the views in
`src/render/`. Anything not listed here is either already implemented or is a
cgit deployment concern (caching, `virtual-root`, the dumb-HTTP `objects`/`info`
endpoints) that doesn't apply to a client-side viewer.

As things are implemented, remove them from this list.

## Whole cgit commands that have no route at all

| cgit URL | What it does | webgit |
|---|---|---|
| `/diff/` | Diff between two arbitrary revisions (`?id=` & `?id2=`), optional path scope | missing — diffs only ever appear inside `#!/commit/<sha>`, against the first parent |
| `/patch/` | `git format-patch` output, plus patch ranges (`?id=..id2`) | partial — the commit page's "(patch)" link downloads the same bytes (`crates/gib-patch`, differential-tested against `git format-patch`), but there is no addressable URL and no ranges |
| `/rawdiff/` | Raw unified diff, no HTML | missing |
| `/plain/` | Raw blob at a URL, with mimetype dispatch | missing — `#!/tree/<path>` gives an in-page download button built from a Blob URL, but there's no addressable raw URL |
| `/blame/` | Per-line blame with commit links | missing |
| `/atom/` | Atom feed of a branch's log | missing |
| `/stats/` | Commit stats per period (week/month/quarter/year), by author | missing |

## Log (`#!/log`)

- **Search** — cgit's `?q=` + `?qt=grep|author|committer|range`. Nothing
  equivalent; there is no search anywhere in webgit.
- **`?showmsg=1`** — expand full commit bodies inline in the log table.
- **`?follow=1`** — follow renames when the log is path-scoped.
- **Revision ranges** as a starting point.
- **Files/Lines columns** (`enable-log-filecount` / `enable-log-linecount`) —
  per-commit diffstat counts in the table. Current columns are
  Age/Commit/Message/Author.

## Tree (`#!/tree`)

- **Size column** — cgit shows blob size; webgit shows only Mode and Name
  (`src/render/tree.rs:171-175`).
- **Per-file links** — cgit puts `log` / `plain` / `blame` links on every row.
- **Submodule links** — no `repo.module-link` equivalent

## Blob

- **Syntax highlighting** (cgit's `source-filter`, e.g. highlight/pygments).
- **HTML serving** (`enable-html-serving`) — deliberately unsafe, probably a
  permanent no.

## Commit (`#!/commit`)

- **Diff controls**: `?context=N`, `?ignorews=1`, `?dt=` (unified / stat-only),
  and `ss=1` **side-by-side diff**.
- **Rename/copy detection** in the diffstat (`git diff -M/-C`), which the
  downloaded patch lacks too — a rename is a delete plus an add.
- **`?id2=`** — diff this commit against something other than its first parent;
  on merges cgit offers a link per parent.

## Refs / tags

- **Sort order config** — cgit can order branches/tags by age or by name.

## Snapshot

- **Formats** — only `.tar.gz`. cgit offers `tar`, `tar.gz`, `tar.bz2`,
  `tar.xz`, `tar.zst`, `zip`.
- **Snapshot prefix config** and `.asc` signature serving.

## Index / repo listing

Currently just section + name derived from the paths in `listing.json`
(`src/render/listing.rs:67-93`). cgit additionally has:

- **Description, owner, homepage** per repo.
- **Idle-time column** ("last commit" age).
- **Sortable columns** — `?s=name|desc|owner|idle`.
- **Repo search/filter** (`?q=`).
- **Quick links per row** (`enable-index-links`: log / files / refs).
- **Root readme** rendered on the index page.
- **`repo.hide` / `repo.ignore`**.

## Site-level config

- **Multiple clone URLs** — webgit shows one, derived from the location
  (`src/lib.rs:130`).
- **Custom header/footer/logo/CSS**, `root-title`/`root-desc`.
- **Filters generally** — `about-filter`, `email-filter` (gravatar),
  `commit-filter`, `auth-filter`. These are server-side pipes in cgit; any
  equivalent would need a different design.

## Feasibility notes

- **Atom feeds** and **auth-filter** are structurally server-side and probably
  out of scope for a static-hosted client.
- **stats**, **blame**, and **log search** are all reachable but expensive: they
  need a full-history walk rather than the bounded fetch webgit currently does,
  so they'd want the commit-graph and some care around request volume.
