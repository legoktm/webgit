use git_async::error::Error as GitError;
use git_async::file_system::FileSystemError;

/// Format a [`FileSystemError`] into a human-readable string.
/// The inner `Box<dyn Any>` is always a `String` in our implementation,
/// so we downcast and display it directly.
pub(crate) fn fmt_fs_err(e: &FileSystemError) -> String {
    let extract = |any: &Box<dyn std::any::Any>| -> String {
        any.downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "(unknown)".to_string())
    };
    match e {
        FileSystemError::NotFound(inner) => format!("not found: {}", extract(inner)),
        FileSystemError::Other(inner) => format!("error: {}", extract(inner)),
    }
}

/// Format a top-level [`GitError`], giving readable messages for filesystem
/// errors (whose `Box<dyn Any>` inner value would otherwise print as `Any { .. }`).
pub(crate) fn fmt_git_err(e: &GitError) -> String {
    match e {
        GitError::FileSystem(fs_err) => fmt_fs_err(fs_err),
        other => format!("{:?}", other),
    }
}
