//! A module for working with git refs
//!
//! A git ref is a file which points to an object or to another ref.
//!
//! The ref types and their parsers live in the `gib-ref` crate and are
//! re-exported here. Resolving a ref against a repository needs a filesystem,
//! so that lives here: [`Repo::lookup_ref`] reads a ref by name, and the
//! peeling methods are in [`crate::prelude`].

use crate::{
    error::{Error, GResult},
    file_system::{Directory, File, FileSystem, FileSystemError, read_file_if_exists},
    repo::Repo,
};

pub use gib_ref::{Ref, RefEntry, RefName, RefTarget};
pub(crate) use gib_ref::{parse_info_refs, parse_packed_refs};

/// Open the loose ref file backing `name`, if the repository has one.
///
/// `HEAD` lives at the top of the git dir; everything else is a path under
/// `refs/`. Returns `None` when any component of that path is missing.
pub(crate) async fn open_loose_ref<F: FileSystem>(
    name: &RefName,
    repo: &Repo<F>,
) -> GResult<Option<F::File>> {
    let sub_path = match name {
        RefName::Head => {
            return Ok(Some(repo.git_dir.open_file(b"HEAD").await?));
        }
        RefName::Ref(path) => path,
    };
    let mut dir = repo.git_dir.open_subdir(b"refs").await?;
    let mut components = sub_path.split(|b| *b == b'/');
    let file_name = components
        .next_back()
        .ok_or_else(|| Error::RefNotFound(name.clone()))?;
    for component in components {
        dir = match dir.open_subdir(component).await {
            Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(dir) => dir,
        };
    }
    match dir.open_file(file_name).await {
        Err(FileSystemError::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
        Ok(file) => Ok(Some(file)),
    }
}

/// Resolve a single ref by name.
///
/// Consult the freshest direct sources first — a loose ref file, then
/// packed-refs — and only then fall back to the info/refs snapshot.
/// The fallback keeps single-ref resolution consistent with
/// [`Repo::all_refs`], which reads info/refs on hosts prepared with
/// `update-server-info`: without it, a ref recorded only in info/refs
/// (e.g. a packed branch on a server that doesn't serve individual loose
/// ref files) would be listed by `all_refs` yet fail to resolve when
/// peeling HEAD.
pub(crate) async fn lookup_ref<F: FileSystem>(repo: &Repo<F>, name: &RefName) -> GResult<Ref> {
    let target = if let Some(target) = lookup_loose_ref(repo, name).await? {
        target
    } else if let Some(target) = lookup_packed_ref(repo, name).await? {
        target
    } else if let Some(target) = lookup_info_ref(repo, name).await? {
        target
    } else {
        return Err(Error::RefNotFound(name.clone()));
    };
    Ok(Ref::new(name.clone(), target))
}

/// Look up a single ref in `packed-refs`, returning `None` when the file is
/// absent or doesn't contain the ref.
async fn lookup_packed_ref<F: FileSystem>(
    repo: &Repo<F>,
    name: &RefName,
) -> GResult<Option<RefTarget>> {
    let Some(data) = read_file_if_exists(&repo.git_dir, b"packed-refs").await? else {
        return Ok(None);
    };
    Ok(parse_packed_refs(&data)?
        .into_iter()
        .find(|(ref_name, _)| ref_name == name)
        .map(|(_, entry)| RefTarget::Direct(entry.target)))
}

/// Look up a single ref in the `info/refs` snapshot written by
/// `git update-server-info`, returning `None` when there is no such file or it
/// doesn't contain the ref.
async fn lookup_info_ref<F: FileSystem>(
    repo: &Repo<F>,
    name: &RefName,
) -> GResult<Option<RefTarget>> {
    let info_dir = match repo.git_dir.open_subdir(b"info").await {
        Ok(dir) => dir,
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(data) = read_file_if_exists(&info_dir, b"refs").await? else {
        return Ok(None);
    };
    Ok(parse_info_refs(&data)?
        .into_iter()
        .find(|(ref_name, _)| ref_name == name)
        .map(|(_, entry)| RefTarget::Direct(entry.target)))
}

pub(crate) async fn lookup_loose_ref<F: FileSystem>(
    repo: &Repo<F>,
    name: &RefName,
) -> GResult<Option<RefTarget>> {
    let Some(mut ref_file) = open_loose_ref(name, repo).await? else {
        return Ok(None);
    };
    let ref_content = match ref_file.read_all().await {
        Ok(content) => content,
        // A lazily-opened ref file that turns out not to exist.
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let (_, ref_type) =
        RefTarget::parse_loose_ref(&ref_content).map_err(|_| Error::MalformedRef(name.clone()))?;
    Ok(Some(ref_type))
}

#[cfg(test)]
mod test {
    use crate::{
        object::Object,
        prelude::*,
        reference::{RefName, RefTarget},
        test::open_test_repo,
    };
    use core::matches;
    use futures::executor::block_on;
    use gib_testkit::{make_basic_repo, make_packfile_repo};

    #[test]
    fn resolve_head() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let head = block_on(repo.head()).unwrap();
        let head_target = match head.target() {
            RefTarget::Direct(_) => panic!(),
            RefTarget::Symbolic(name) => name.clone(),
        };
        let head_target = block_on(repo.lookup_ref(&head_target)).unwrap();
        assert!(matches!(head_target.target(), RefTarget::Direct(_)));
    }

    #[test]
    fn read_thin_packed_ref() {
        let test_repo = make_packfile_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let ref_name = RefName::Ref(b"heads/main".to_vec());
        let reference = block_on(repo.lookup_ref(&ref_name)).unwrap();
        let oid = block_on(reference.resolve_object_id(&repo)).unwrap();
        let object = block_on(repo.lookup_object(oid)).unwrap();
        assert!(matches!(object, Object::Commit(_)));
    }

    #[test]
    fn read_fat_packed_ref() {
        let test_repo = make_packfile_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let ref_name = RefName::Ref(b"tags/a-fat-tag".to_vec());
        let reference = block_on(repo.lookup_ref(&ref_name)).unwrap();
        let oid = block_on(reference.resolve_object_id(&repo)).unwrap();
        let object = block_on(repo.lookup_object(oid)).unwrap();
        assert!(matches!(object, Object::Tag(_)));
    }

    #[test]
    fn lookup_ref_falls_back_to_info_refs() {
        let test_repo = make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        // Remove packed-refs so heads/main lives only in info/refs (it has no
        // loose ref file either, having been packed). This mirrors a server
        // that lists refs via info/refs but won't serve them individually.
        std::fs::remove_file(test_repo.location.path().join(".git").join("packed-refs")).unwrap();

        let repo = open_test_repo(&test_repo);
        let ref_name = RefName::Ref(b"heads/main".to_vec());
        let reference = block_on(repo.lookup_ref(&ref_name)).unwrap();
        let oid = block_on(reference.resolve_object_id(&repo)).unwrap();
        let object = block_on(repo.lookup_object(oid)).unwrap();
        assert!(matches!(object, Object::Commit(_)));
    }
}
