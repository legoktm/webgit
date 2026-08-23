Reading `.mailmap` and rewriting one contact with it: the file format (the four
line shapes git accepts), the case-insensitive lookup on email and then name,
and which half of a contact each kind of entry replaces. Checked against
`git check-mailmap`.

The crate does no IO.
