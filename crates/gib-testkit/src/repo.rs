use crate::directory::{TestRepoDirectory, TestRepoFile};

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Seek, SeekFrom},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};
use tempfile::{TempDir, tempdir, tempfile};

/// Where a test repository lives on disk.
#[derive(Debug, Clone)]
pub enum TestDirectory {
    /// A temporary directory, removed when the last handle is dropped.
    Temp(Arc<TempDir>),

    /// This is for debugging operations on real repos, the tests for which are
    /// not to be committed.
    #[allow(dead_code)]
    Real(PathBuf),
}

impl TestDirectory {
    /// The directory's path on disk.
    pub fn path(&self) -> &Path {
        use TestDirectory::*;
        match self {
            Temp(d) => d.path(),
            Real(d) => d.as_path(),
        }
    }

    #[allow(dead_code)]
    /// Keep the test directory around for debugging
    pub fn forget(&self) {
        use TestDirectory::*;
        match self {
            Temp(d) => {
                std::mem::forget(d.clone());
                println!("{:?}", d.path());
            }
            Real(_) => {}
        }
    }
}

/// A real git repository, built by shelling out to the `git` CLI.
#[derive(Debug)]
pub struct TestRepo {
    /// Where the repository's working tree (and `.git`) lives.
    pub location: TestDirectory,
}

/// The empty environment for [`TestRepo::run_git_with_env`], spelled out
/// because an empty iterator literal gives the key and value types nowhere to
/// be inferred from.
const NO_ENV: [(&str, &str); 0] = [];

impl TestRepo {
    /// A `git` invocation in this repository, with the ambient identity and
    /// date environment cleared so tests are reproducible.
    pub fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(self.location.path())
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_AUTHOR_DATE")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("GIT_COMMITTER_DATE");
        command
    }
    pub fn run_git(
        &self,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> io::Result<Vec<u8>> {
        self.run_git_with_env(NO_ENV, args)
    }

    /// [`TestRepo::run_git`] with extra environment variables set for the one
    /// invocation. `GIT_INDEX_FILE` is the reason this exists: plumbing that
    /// builds a tree (`update-index` then `write-tree`) needs somewhere to
    /// build it that isn't the repository's real index.
    pub fn run_git_with_env(
        &self,
        env: impl IntoIterator<Item = (impl AsRef<OsStr>, impl AsRef<OsStr>)>,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> io::Result<Vec<u8>> {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        // Send stderr to a file rather than a pipe: a pipe would have to be
        // drained concurrently with stdout to avoid deadlocking on a chatty
        // command, and keeping it lets a failure report what git complained
        // about.
        let mut stderr_file = tempfile()?;
        let mut command = self.git_command();
        for (key, value) in env {
            command.env(key, value);
        }
        let mut git_process = command
            .args(&args)
            .stderr(Stdio::from(stderr_file.try_clone()?))
            .spawn()?;
        // Close git's stdin so anything that would read from it sees EOF, then
        // drain stdout *before* waiting. Waiting first deadlocks as soon as the
        // command writes more than a pipe buffer's worth of output, which the
        // bulk queries in the differential suite comfortably do.
        drop(git_process.stdin.take());
        let mut output = Vec::new();
        git_process
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut output)?;
        let status = git_process.wait()?;
        if !status.success() {
            let mut stderr = String::new();
            stderr_file.seek(SeekFrom::Start(0))?;
            stderr_file.read_to_string(&mut stderr)?;
            panic!("`git {args:?}` failed with {status}:\n{stderr}");
        }
        Ok(output)
    }

    pub fn new() -> io::Result<Self> {
        let dir = tempdir()?;
        let repo = TestRepo {
            location: TestDirectory::Temp(Arc::new(dir)),
        };
        repo.run_git(["init", "--initial-branch=main"])?;
        repo.set_user("a user", "an-email-address")?;
        // Keep the repository's on-disk shape entirely under the test's
        // control. Otherwise git runs auto-maintenance behind `git commit`,
        // which packs loose objects and refreshes `info/refs` at unpredictable
        // moments — so a test that means to exercise loose reading may not, and
        // a background repack can race an explicit `git gc`.
        repo.run_git(["config", "gc.auto", "0"])?;
        repo.run_git(["config", "maintenance.auto", "false"])?;
        Ok(repo)
    }

    fn set_user(&self, name: &str, email: &str) -> io::Result<()> {
        self.run_git(["config", "user.name", name])?;
        self.run_git(["config", "user.email", email])?;
        Ok(())
    }

    pub fn root_dir(&self) -> TestRepoDirectory {
        TestRepoDirectory {
            root: self.location.clone(),
            sub_path: PathBuf::new(),
        }
    }

    pub fn git_dir(&self) -> TestRepoDirectory {
        TestRepoDirectory {
            root: self.location.clone(),
            sub_path: PathBuf::from(".git"),
        }
    }

    pub fn commit(
        &self,
        message: &str,
        author_name: &str,
        author_email: &str,
        date: &str,
    ) -> io::Result<()> {
        self.set_user(author_name, author_email)?;
        let mut p = self
            .git_command()
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .args(["commit", "-m", message])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let status = p.wait().unwrap();
        assert!(status.success());
        Ok(())
    }

    pub fn tag_annotated(
        &self,
        tag_name: &str,
        object: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
        date: &str,
    ) -> io::Result<()> {
        self.set_user(author_name, author_email)?;
        let mut p = self
            .git_command()
            .env("GIT_COMMITTER_DATE", date)
            .args(["tag", "-a", "-m", message, tag_name, object])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let status = p.wait().unwrap();
        assert!(status.success());
        Ok(())
    }

    fn pack_dir_path(&self) -> PathBuf {
        self.location
            .path()
            .join(".git")
            .join("objects")
            .join("pack")
            .clone()
    }

    pub fn pack_idx_file(&self, pack_id: &[u8]) -> TestRepoFile {
        let mut idx_name = Vec::new();
        idx_name.extend_from_slice(b"pack-");
        idx_name.extend_from_slice(pack_id);
        idx_name.extend_from_slice(b".idx");
        TestRepoFile {
            path: self.pack_dir_path().join(OsStr::from_bytes(&idx_name)),
            _dir: self.location.clone(),
        }
    }
}
