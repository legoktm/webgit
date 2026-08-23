//! Reading the notes attached to an object.
//!
//! A notes ref (`refs/notes/commits` by default) points at a commit whose tree
//! maps an annotated object's hex ID to a blob holding the note's text. The
//! mapping is not flat: git splits the ID across a "fanout" of directories
//! named with two hex characters each, deepening the tree as the number of
//! notes grows (`2/38`, then `2/2/36`, and so on), so that no single tree
//! object has to be rewritten in full for every added note.
//!
//! Reading that back only needs the shape, not the rewriting rules: at each
//! level git's own loader ([`load_subtree`] in `notes.c`) recognises exactly
//! two kinds of entry — a two-character *directory*, or a *blob* named with the
//! entire remaining hex — and treats everything else as not part of the notes
//! mapping. [`lookup_note`] walks those two cases down from the root.
//!
//! [`load_subtree`]: https://github.com/git/git/blob/master/notes.c

use crate::{
    error::{Error, GResult},
    file_system::FileSystem,
    object::{Object, ObjectId, Tree, TreeEntryType},
    prelude::*,
    reference::RefName,
    repo::Repo,
};

/// The ref git reads notes from unless `core.notesRef` says otherwise.
///
/// Named without the leading `refs/`, as [`RefName::Ref`] is throughout.
pub fn default_notes_ref() -> RefName {
    RefName::Ref(b"notes/commits".to_vec())
}

/// What a notes tree entry can be, once its name has been matched against the
/// ID being looked up. Carrying the ID out by value (rather than the entry)
/// lets the caller replace the tree it was found in.
enum NoteEntry {
    /// The note itself: a blob holding the text.
    Note(ObjectId),
    /// One fanout level: a tree to continue the walk in.
    Subtree(ObjectId),
}

/// Match the entries of one notes tree against `remaining`, the still-unmatched
/// tail of the hex ID at this level.
fn find_note_entry(tree: &Tree, remaining: &[u8]) -> Option<NoteEntry> {
    let mut subtree = None;
    for entry in tree.entries() {
        if entry.name() == remaining {
            if matches!(
                entry.entry_type(),
                TreeEntryType::File | TreeEntryType::Executable
            ) {
                return Some(NoteEntry::Note(entry.id()));
            }
        } else if remaining.len() > 2
            && entry.name() == &remaining[..2]
            && entry.entry_type() == TreeEntryType::Tree
        {
            // Keep looking rather than descending straight away: a note for
            // this exact ID, if the tree also holds one, is the better match.
            subtree = Some(NoteEntry::Subtree(entry.id()));
        }
    }
    subtree
}

/// Find the note attached to `target` in the notes tree rooted at `root`,
/// returning the note's bytes, or `None` when the tree holds no note for it.
pub async fn lookup_note<E: From<Error>>(
    root: &Tree,
    target: ObjectId,
    mut lookup: impl AsyncFnMut(ObjectId) -> Result<Object, E>,
) -> Result<Option<Vec<u8>>, E> {
    let hex = target.to_string();
    let hex = hex.as_bytes();
    let mut tree = root.clone();
    let mut consumed = 0;
    // Bounded by the hash: each level consumes two characters of it, and a
    // fanout directory is only taken when at least one character would be left
    // for the leaf, so the walk cannot outlast the ID.
    while consumed < hex.len() {
        match find_note_entry(&tree, &hex[consumed..]) {
            None => return Ok(None),
            Some(NoteEntry::Note(id)) => {
                let blob = lookup(id)
                    .await?
                    .blob()
                    .map_err(|e| E::from(Error::from(e)))?;
                return Ok(Some(blob.data_owned()));
            }
            Some(NoteEntry::Subtree(id)) => {
                tree = lookup(id)
                    .await?
                    .tree()
                    .map_err(|e| E::from(Error::from(e)))?;
                consumed += 2;
            }
        }
    }
    Ok(None)
}

/// Read the tree a notes ref points at, or `None` when the repository has no
/// such ref.
pub(crate) async fn notes_root<F: FileSystem>(
    repo: &Repo<F>,
    notes_ref: &RefName,
) -> GResult<Option<Tree>> {
    let target = match repo.lookup_ref(notes_ref).await {
        Ok(r) => r.resolve_object_id(repo).await?,
        Err(Error::RefNotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    repo.lookup_object(target).await?.peel_to_tree(repo).await
}

/// The note attached to `target`, read from the repository's default notes ref.
pub(crate) async fn note<F: FileSystem>(
    repo: &Repo<F>,
    target: ObjectId,
) -> GResult<Option<Vec<u8>>> {
    let Some(root) = notes_root(repo, &default_notes_ref()).await? else {
        return Ok(None);
    };
    lookup_note(&root, target, async |id| repo.lookup_object(id).await).await
}

#[cfg(test)]
mod test {
    use crate::{notes::default_notes_ref, object::ObjectId, test::open_test_repo};
    use futures::executor::block_on;
    use gib_testkit::{TestRepo, make_basic_repo};

    /// Run `git` and take its output as a trimmed string — plumbing that prints
    /// one object ID per line, in every use here.
    fn run(test_repo: &TestRepo, args: &[&str]) -> String {
        let out = test_repo.run_git(args).unwrap();
        String::from_utf8(out).unwrap().trim_end().to_string()
    }

    fn head_oid(test_repo: &TestRepo) -> ObjectId {
        ObjectId::from_hex(run(test_repo, &["rev-parse", "HEAD"]).as_bytes()).unwrap()
    }

    /// Write `content` into the object database and return its blob ID.
    fn hash_blob(test_repo: &TestRepo, content: &str) -> String {
        std::fs::write(test_repo.location.path().join("scratch-blob"), content).unwrap();
        run(test_repo, &["hash-object", "-w", "--", "scratch-blob"])
    }

    /// Point `refs/notes/commits` at a tree holding exactly `entries`, given as
    /// `(mode, path, content)`.
    fn write_notes_tree(test_repo: &TestRepo, entries: &[(&str, &str, &str)]) {
        let index = [("GIT_INDEX_FILE", "notes-index")];
        for (mode, path, content) in entries {
            let blob = hash_blob(test_repo, content);
            let cacheinfo = format!("{mode},{blob},{path}");
            test_repo
                .run_git_with_env(index, ["update-index", "--add", "--cacheinfo", &cacheinfo])
                .unwrap();
        }
        let tree = test_repo.run_git_with_env(index, ["write-tree"]).unwrap();
        let tree = String::from_utf8(tree).unwrap().trim_end().to_string();
        let commit = run(test_repo, &["commit-tree", &tree, "-m", "notes"]);
        run(test_repo, &["update-ref", "refs/notes/commits", &commit]);
    }

    /// The note git itself reads for `oid`, as raw blob bytes, or `None` when
    /// git finds none. Every layout this suite builds by hand is checked
    /// against this too, so a walk that agreed with the test but not with git
    /// would not pass.
    fn git_note(test_repo: &TestRepo, oid: ObjectId) -> Option<Vec<u8>> {
        // The whole mapping rather than one lookup: asking for a single object
        // exits non-zero when there is no note, which `run_git` treats as git
        // having failed. Listing everything also puts git's loader over the
        // entire tree, so a layout it rejects shows up here as an absence.
        let hex = oid.to_string();
        let blob = run(test_repo, &["notes", "list"])
            .lines()
            .find_map(|line| {
                let (blob, object) = line.split_once(' ')?;
                (object == hex).then(|| blob.to_string())
            })?;
        Some(test_repo.run_git(["cat-file", "blob", &blob]).unwrap())
    }

    #[test]
    fn reads_a_note_written_by_git() {
        let test_repo = make_basic_repo().unwrap();
        run(&test_repo, &["notes", "add", "-m", "a note"]);
        let repo = open_test_repo(&test_repo);
        let oid = head_oid(&test_repo);

        let note = block_on(repo.note(oid)).unwrap();
        assert_eq!(note.as_deref(), Some(&b"a note\n"[..]));
        assert_eq!(note, git_note(&test_repo, oid));
    }

    #[test]
    fn a_repository_without_notes_has_no_note() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        assert_eq!(block_on(repo.note(head_oid(&test_repo))).unwrap(), None);
    }

    #[test]
    fn an_object_with_no_note_of_its_own() {
        let test_repo = make_basic_repo().unwrap();
        run(&test_repo, &["notes", "add", "-m", "a note"]);
        let repo = open_test_repo(&test_repo);
        // The commit's tree is a real object in the repository, and one the
        // notes tree says nothing about.
        let tree =
            ObjectId::from_hex(run(&test_repo, &["rev-parse", "HEAD^{tree}"]).as_bytes()).unwrap();

        assert_eq!(block_on(repo.note(tree)).unwrap(), None);
        assert_eq!(git_note(&test_repo, tree), None);
    }

    #[test]
    fn notes_read_from_a_packfile() {
        let test_repo = make_basic_repo().unwrap();
        run(&test_repo, &["notes", "add", "-m", "a packed note"]);
        // Packs the notes commit, tree and blob, and moves the notes ref into
        // packed-refs, so nothing about this note is left as a loose file.
        run(&test_repo, &["gc"]);
        run(&test_repo, &["pack-refs", "--all"]);
        let repo = open_test_repo(&test_repo);
        let oid = head_oid(&test_repo);

        assert_eq!(
            block_on(repo.note(oid)).unwrap().as_deref(),
            Some(&b"a packed note\n"[..])
        );
    }

    #[test]
    fn walks_a_two_level_fanout() {
        let test_repo = make_basic_repo().unwrap();
        let oid = head_oid(&test_repo);
        let hex = oid.to_string();
        let path = format!("{}/{}", &hex[..2], &hex[2..]);
        write_notes_tree(&test_repo, &[("100644", &path, "fanned")]);
        let repo = open_test_repo(&test_repo);

        assert_eq!(
            block_on(repo.note(oid)).unwrap().as_deref(),
            Some(&b"fanned"[..])
        );
        assert_eq!(git_note(&test_repo, oid).as_deref(), Some(&b"fanned"[..]));
    }

    #[test]
    fn walks_a_three_level_fanout() {
        let test_repo = make_basic_repo().unwrap();
        let oid = head_oid(&test_repo);
        let hex = oid.to_string();
        let path = format!("{}/{}/{}", &hex[..2], &hex[2..4], &hex[4..]);
        write_notes_tree(&test_repo, &[("100644", &path, "deeply fanned")]);
        let repo = open_test_repo(&test_repo);

        assert_eq!(
            block_on(repo.note(oid)).unwrap().as_deref(),
            Some(&b"deeply fanned"[..])
        );
        assert_eq!(
            git_note(&test_repo, oid).as_deref(),
            Some(&b"deeply fanned"[..])
        );
    }

    #[test]
    fn a_fanout_name_that_is_a_blob_is_not_a_note() {
        let test_repo = make_basic_repo().unwrap();
        let oid = head_oid(&test_repo);
        let hex = oid.to_string();
        // A file whose name happens to be this ID's first fanout level. Git
        // treats it as somebody else's file living in the notes tree; so must
        // we, rather than trying to read it as a fanout directory.
        write_notes_tree(&test_repo, &[("100644", &hex[..2], "not a fanout level")]);
        let repo = open_test_repo(&test_repo);

        assert_eq!(block_on(repo.note(oid)).unwrap(), None);
        assert_eq!(git_note(&test_repo, oid), None);
    }

    #[test]
    fn a_note_name_that_is_a_symlink_is_not_a_note() {
        let test_repo = make_basic_repo().unwrap();
        let oid = head_oid(&test_repo);
        // Git requires a note to be a regular file (`S_ISREG`), which rules out
        // a symlink pointing at one.
        write_notes_tree(&test_repo, &[("120000", &oid.to_string(), "elsewhere")]);
        let repo = open_test_repo(&test_repo);

        assert_eq!(block_on(repo.note(oid)).unwrap(), None);
        assert_eq!(git_note(&test_repo, oid), None);
    }

    #[test]
    fn a_notes_root_is_the_tree_the_ref_points_at() {
        let test_repo = make_basic_repo().unwrap();
        run(&test_repo, &["notes", "add", "-m", "a note"]);
        let repo = open_test_repo(&test_repo);
        let root = block_on(repo.notes_root(&default_notes_ref())).unwrap();
        let expected = run(&test_repo, &["rev-parse", "refs/notes/commits^{tree}"]);
        assert_eq!(root.map(|t| t.id().to_string()), Some(expected));
    }
}
