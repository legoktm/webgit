Pure parsers and types for git's four object kinds — commit, tree, tag, and
blob — plus the loose-object header and the author/committer/tagger date
lines. Input is decompressed object bytes; there is no IO and no knowledge of
where objects are stored. Fetching objects lives in `gib-odb`, and peeling
(which requires lookups) lives in the facade's extension traits.
