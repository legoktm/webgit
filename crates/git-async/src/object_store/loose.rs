use gib_object::parse_header;
use miniz_oxide::inflate::decompress_to_vec_zlib;

use crate::{
    error::{Error, GResult},
    file_system::{Directory, File, FileSystem, FileSystemError},
    object::ObjectId,
    object_store::RawObject,
    repo::Repo,
};

async fn get_loose_object_file<F: FileSystem>(
    repo: &Repo<F>,
    id: ObjectId,
) -> GResult<Option<F::File>> {
    let (prefix, suffix) = id.bytes().split_at(1);
    let mut prefix_buf = [0u8; 2];
    hex::encode_to_slice(prefix, &mut prefix_buf).unwrap();
    let mut suffix_buf = [0u8; 2 * 19];
    hex::encode_to_slice(suffix, &mut suffix_buf).unwrap();
    let mut dir = repo.git_dir.open_subdir(b"objects").await?;
    dir = match dir.open_subdir(&prefix_buf).await {
        Ok(d) => d,
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let file = match dir.open_file(&suffix_buf).await {
        Ok(f) => f,
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(file))
}

pub(crate) async fn read_loose_object<F: FileSystem>(
    repo: &Repo<F>,
    id: ObjectId,
) -> GResult<Option<RawObject>> {
    let file = get_loose_object_file(repo, id).await?;
    let Some(mut file) = file else {
        return Ok(None);
    };
    let data = match file.read_all().await {
        Ok(data) => data,
        // A lazily-opened object file that turns out not to exist.
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let data = decompress_to_vec_zlib(&data).map_err(|e| Error::LooseObjectDecompressError {
        id,
        status: e.status,
    })?;
    let (body, (_, object_type)) = parse_header(&data).map_err(|_| Error::MalformedObject(id))?;
    Ok(Some(RawObject {
        object_type,
        body: body.to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use crate::{object_store::ObjectType, test::open_test_repo};
    use futures::executor::block_on;
    use gib_testkit::make_basic_repo;
    use hex_literal::hex;

    use super::*;

    #[test]
    fn test_read_loose_object_existing() {
        let test_repo = make_basic_repo().unwrap();
        let commit_id = test_repo.run_git(["rev-parse", "HEAD"]).unwrap();
        let commit_id = ObjectId::from_hex(commit_id.trim_ascii()).unwrap();

        let repo = open_test_repo(&test_repo);
        let object = block_on(read_loose_object(&repo, commit_id))
            .unwrap()
            .unwrap();
        assert_eq!(object.object_type, ObjectType::Commit);
        assert_eq!(
            object.body,
            b"tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb
author a user <an-email-address> 946684800 +0000
committer a user <an-email-address> 946684800 +0000

a commit
"
        );
    }

    #[test]
    fn test_read_loose_object_nonexistent() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let object = block_on(read_loose_object(
            &repo,
            ObjectId::from_bytes(hex!("0000000000000000000000000000000000000000")),
        ))
        .unwrap();
        assert!(object.is_none());
    }
}
