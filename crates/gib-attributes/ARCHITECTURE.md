Reading `.gitattributes` and resolving one attribute for one path: the file
format (patterns plus `attr` / `-attr` / `!attr` / `attr=value` assignments),
the stack of files a directory inherits, and git's precedence between them —
innermost file first, last matching line within it. Checked against
`git check-attr`.

The pattern matcher is a port of git's `wildmatch.c`, checked against the corpus
in git's own `t/t3070-wildmatch.sh`. It is private to the crate; if gitignore
support ever wants it, that is the thing to lift out rather than reimplement.

Deliberately not here: macro (`[attr]`) expansion, attribute sources outside the
tree (`info/attributes`, the global and system files, git's built-in set), and
case folding. The crate does no IO — a caller hands it the bytes of each file it
found, which is what lets the browser feed it blobs it fetched over HTTP.
