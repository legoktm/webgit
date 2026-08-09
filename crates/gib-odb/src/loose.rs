use crate::{OdbError, OdbResult};
use gib_fs::{Directory, File, FileSystem, FileSystemError};
use gib_hash::ObjectId;
use gib_object::{RawObject, parse_header};
use miniz_oxide::inflate::decompress_to_vec_zlib;

/// Open `objects/ab/cdef…` for `id`, or `None` if it is not there.
async fn get_loose_object_file<F: FileSystem>(
    objects_dir: &F::Directory,
    id: ObjectId,
) -> OdbResult<Option<F::File>> {
    let (prefix, suffix) = id.bytes().split_at(1);
    let mut prefix_buf = [0u8; 2];
    hex::encode_to_slice(prefix, &mut prefix_buf).unwrap();
    let mut suffix_buf = [0u8; 2 * 19];
    hex::encode_to_slice(suffix, &mut suffix_buf).unwrap();
    let dir = match objects_dir.open_subdir(&prefix_buf).await {
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
    objects_dir: &F::Directory,
    id: ObjectId,
) -> OdbResult<Option<RawObject>> {
    let file = get_loose_object_file::<F>(objects_dir, id).await?;
    let Some(mut file) = file else {
        return Ok(None);
    };
    let data = match file.read_all().await {
        Ok(data) => data,
        // A lazily-opened object file that turns out not to exist.
        Err(FileSystemError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let data = decompress_to_vec_zlib(&data).map_err(|e| OdbError::LooseObjectDecompressError {
        id,
        status: e.status,
    })?;
    let (body, (_, object_type)) =
        parse_header(&data).map_err(|_| OdbError::MalformedObject(id))?;
    Ok(Some(RawObject {
        object_type,
        body: body.to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::objects_dir;
    use futures::executor::block_on;
    use gib_object::ObjectType;
    use gib_testkit::{TestFileSystem, make_basic_repo};
    use hex_literal::hex;

    #[test]
    fn test_read_loose_object_existing() {
        let test_repo = make_basic_repo().unwrap();
        let commit_id = test_repo.run_git(["rev-parse", "HEAD"]).unwrap();
        let commit_id = ObjectId::from_hex(commit_id.trim_ascii()).unwrap();

        let object = block_on(read_loose_object::<TestFileSystem>(
            &objects_dir(&test_repo),
            commit_id,
        ))
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
        let object = block_on(read_loose_object::<TestFileSystem>(
            &objects_dir(&test_repo),
            ObjectId::from_bytes(hex!("0000000000000000000000000000000000000000")),
        ))
        .unwrap();
        assert!(object.is_none());
    }
}
