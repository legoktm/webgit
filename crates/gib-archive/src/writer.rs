//! Writing the entries out as a tar, in the shape `git archive --format=tar`
//! writes one.
//!
//! Everything here is about matching git byte for byte: the modes it
//! normalises to, the `pax_global_header` it opens with, and the padding it
//! ends with. [`TarWriter`] hands the archive back a piece at a time rather
//! than whole, so a caller compressing it never has to hold both halves —
//! `build_tar` in the tests is the same writer driven to completion in one go,
//! and is what pins the agreement with git.

use crate::{ArchiveEntry, EntryKind};

/// Tar's record size: archives are padded out to a multiple of this, as git's
/// (and GNU tar's) are.
const RECORD_SIZE: usize = 512 * 20;

/// `git archive` normalises every mode to one of these before writing it —
/// permissions in a git tree only really record the executable bit, so the rest
/// is invented, and git invents 0666/0777 minus its default `tar.umask` of 002.
/// Reproducing that is what makes the output comparable to git's.
const MODE_FILE: u32 = 0o664;
const MODE_EXEC: u32 = 0o775;
const MODE_DIR: u32 = 0o775;
const MODE_LINK: u32 = 0o777;

/// A tar being written, one entry at a time.
///
/// The archive is buffered rather than streamed straight out: [`take`] hands
/// back what has accumulated so far, so a caller compressing the tar can push
/// it onward in whatever sized pieces it likes and drop each one, and
/// [`finish`] closes the archive off. The writer remembers how much has already
/// been taken, which is what the final padding needs — git (like GNU tar) pads
/// the *whole* archive to a multiple of `RECORD_SIZE`, a property of the
/// total length rather than of the last piece.
///
/// [`take`]: TarWriter::take
/// [`finish`]: TarWriter::finish
pub struct TarWriter {
    builder: tar::Builder<Vec<u8>>,
    /// The directory every entry is placed under, git's `--prefix`, trailing
    /// slash included.
    prefix: String,
    /// Stamped on every entry — the commit's own time, so that archiving the
    /// same commit twice yields the same bytes.
    mtime: u64,
    /// How much of the archive has already been handed out by [`take`].
    ///
    /// [`take`]: TarWriter::take
    emitted: usize,
}

impl TarWriter {
    /// Open an archive, writing its first two records: the pax global header
    /// carrying `commit`, then the prefix directory itself.
    ///
    /// The global header is the one `git archive` writes and
    /// `git get-tar-commit-id` reads back out. Its record is a pax keyword line
    /// whose leading number counts its own bytes: `comment=<id>\n` plus the
    /// digits of the length plus the separating space.
    pub fn new(prefix: &str, commit: &str, mtime: u64) -> std::io::Result<Self> {
        let mut builder = tar::Builder::new(Vec::new());
        let comment = format!("comment={commit}\n");
        let record = format!("{} {}", comment.len() + 3, comment);
        let mut header = tar_header(mtime);
        header.set_mode(0o666);
        header.set_size(record.len() as u64);
        header.set_entry_type(tar::EntryType::XGlobalHeader);
        builder.append_data(&mut header, "pax_global_header", record.as_bytes())?;
        append(&mut builder, prefix, &EntryKind::Directory, &[], mtime)?;
        Ok(Self {
            builder,
            prefix: prefix.to_string(),
            mtime,
            emitted: 0,
        })
    }

    /// Write one entry, under the archive's prefix.
    pub fn append(&mut self, entry: &ArchiveEntry) -> std::io::Result<()> {
        let path = format!("{}{}", self.prefix, entry.path);
        append(
            &mut self.builder,
            &path,
            &entry.kind,
            &entry.data,
            self.mtime,
        )
    }

    /// How much tar has accumulated since the last [`take`], which is what a
    /// caller flushing on a size threshold watches.
    ///
    /// [`take`]: TarWriter::take
    pub fn pending(&self) -> usize {
        self.builder.get_ref().len()
    }

    /// Take everything written so far, leaving the writer empty and ready for
    /// more entries.
    pub fn take(&mut self) -> Vec<u8> {
        let chunk = std::mem::take(self.builder.get_mut());
        self.emitted += chunk.len();
        chunk
    }

    /// Close the archive, returning whatever is left to write: the two
    /// end-of-archive records and the padding out to a whole `RECORD_SIZE`.
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        let emitted = self.emitted;
        let mut tail = self.builder.into_inner()?;
        let remainder = (emitted + tail.len()) % RECORD_SIZE;
        if remainder != 0 {
            tail.resize(tail.len() + (RECORD_SIZE - remainder), 0);
        }
        Ok(tail)
    }
}

/// Build the whole tar in memory. Only the tests want this — what ships takes
/// the archive a piece at a time and gzips it as it goes — but it is what pins
/// the byte-for-byte agreement with `git archive`.
#[cfg(test)]
fn build_tar(
    entries: &[ArchiveEntry],
    prefix: &str,
    commit: &str,
    mtime: u64,
) -> std::io::Result<Vec<u8>> {
    let mut writer = TarWriter::new(prefix, commit, mtime)?;
    for entry in entries {
        writer.append(entry)?;
    }
    let mut out = writer.take();
    out.extend_from_slice(&writer.finish()?);
    Ok(out)
}

/// A header with the fields git fills in identically for every entry: no owner,
/// named `root`, and explicit (rather than left-empty) device numbers.
fn tar_header(mtime: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_uid(0);
    header.set_gid(0);
    // Both names fit, and the header is ustar, so neither call can fail.
    header.set_username("root").expect("username fits");
    header.set_groupname("root").expect("groupname fits");
    header.set_mtime(mtime);
    header.set_device_major(0).expect("ustar header");
    header.set_device_minor(0).expect("ustar header");
    header
}

fn append(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    kind: &EntryKind,
    data: &[u8],
    mtime: u64,
) -> std::io::Result<()> {
    let mut header = tar_header(mtime);
    match kind {
        EntryKind::Directory => {
            header.set_mode(MODE_DIR);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            // A directory member's name carries the trailing slash.
            builder.append_data(&mut header, format!("{path}/"), &[][..])
        }
        EntryKind::Symlink { target } => {
            header.set_mode(MODE_LINK);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            // `append_link` rather than `set_link_name` + `append_data`: a
            // target too long for the 100-byte ustar linkname field only fits
            // behind a GNU longlink record, and `append_link` is what emits
            // one. Vendored and nix-style trees hit that limit, and the whole
            // download used to fail on the first such symlink. It is the same
            // fallback `append_data` already gives an over-long entry *path*.
            builder.append_link(&mut header, path, String::from_utf8_lossy(target).as_ref())
        }
        EntryKind::File { executable } => {
            header.set_mode(if *executable { MODE_EXEC } else { MODE_FILE });
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, path, data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn file(path: &str, data: &str) -> ArchiveEntry {
        ArchiveEntry {
            path: path.to_string(),
            kind: EntryKind::File { executable: false },
            data: data.as_bytes().to_vec(),
        }
    }

    fn fixture() -> Vec<ArchiveEntry> {
        vec![
            file("README.md", "hi\n"),
            ArchiveEntry {
                path: "link.md".to_string(),
                kind: EntryKind::Symlink {
                    target: b"README.md".to_vec(),
                },
                data: Vec::new(),
            },
            ArchiveEntry {
                path: "run.sh".to_string(),
                kind: EntryKind::File { executable: true },
                data: b"#!/bin/sh\n".to_vec(),
            },
            ArchiveEntry {
                path: "sub".to_string(),
                kind: EntryKind::Directory,
                data: Vec::new(),
            },
            file("sub/a.txt", "x\n"),
        ]
    }

    /// Read an archive back into `(path, mode, type, link target, size)` rows.
    fn entries_of(tar_bytes: &[u8]) -> Vec<(String, u32, u8, Option<String>, u64)> {
        let mut archive = tar::Archive::new(tar_bytes);
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let h = e.header();
                (
                    e.path().unwrap().to_string_lossy().into_owned(),
                    h.mode().unwrap(),
                    h.entry_type().as_byte(),
                    // From the entry, not the header: a target too long for the
                    // header field lives in a preceding GNU longlink record,
                    // and only the entry knows to look there — same reason the
                    // path above comes from `e.path()`.
                    e.link_name()
                        .unwrap()
                        .map(|p| p.to_string_lossy().into_owned()),
                    h.size().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn test_tar_entries() {
        let tar = build_tar(&fixture(), "demo-main/", &"a".repeat(40), 1_700_000_000).unwrap();
        let got = entries_of(&tar);
        let names: Vec<&str> = got.iter().map(|(p, ..)| p.as_str()).collect();
        assert_eq!(
            names,
            [
                "pax_global_header",
                "demo-main/",
                "demo-main/README.md",
                "demo-main/link.md",
                "demo-main/run.sh",
                "demo-main/sub/",
                "demo-main/sub/a.txt",
            ]
        );
        // Modes and types: dir, regular, symlink (with its target), executable.
        assert_eq!(got[1].1, MODE_DIR);
        assert_eq!(got[1].2, b'5');
        assert_eq!(got[2].1, MODE_FILE);
        assert_eq!(got[3].2, b'2');
        assert_eq!(got[3].3.as_deref(), Some("README.md"));
        assert_eq!(got[4].1, MODE_EXEC);
        assert_eq!(got[2].4, 3);
    }

    /// A path too long for a ustar header still round-trips, via the GNU
    /// long-name entry the tar crate falls back to.
    #[test]
    fn test_long_path_round_trips() {
        let long = format!("{}/deep.txt", "a-long-directory-name".repeat(8));
        let tar = build_tar(
            &[file(&long, "deep\n")],
            "demo-main/",
            &"b".repeat(40),
            1_700_000_000,
        )
        .unwrap();
        let got = entries_of(&tar);
        assert!(
            got.iter().any(|(p, ..)| *p == format!("demo-main/{long}")),
            "long path missing from {:?}",
            got.iter().map(|(p, ..)| p).collect::<Vec<_>>()
        );
    }

    /// A symlink target too long for a ustar header round-trips the same way,
    /// via a GNU longlink entry. Vendored and nix-style trees carry targets
    /// well past the 100-byte field, and one of them used to fail the whole
    /// download rather than just its own entry.
    #[test]
    fn test_long_symlink_target_round_trips() {
        let target = format!("{}/README.md", "../a-long-directory-name".repeat(6));
        assert!(target.len() > 100, "the target has to overflow the field");
        let tar = build_tar(
            &[ArchiveEntry {
                path: "link.md".to_string(),
                kind: EntryKind::Symlink {
                    target: target.clone().into_bytes(),
                },
                data: Vec::new(),
            }],
            "demo-main/",
            &"c".repeat(40),
            1_700_000_000,
        )
        .unwrap();
        let link = entries_of(&tar)
            .into_iter()
            .find(|(path, ..)| path == "demo-main/link.md")
            .expect("the symlink is missing from the archive");
        assert_eq!(link.2, b'2');
        assert_eq!(link.3.as_deref(), Some(target.as_str()));
    }

    /// The whole point of the mode normalisation, the global header and the
    /// record padding: what we write is what `git archive --format=tar` writes,
    /// byte for byte, for the same commit.
    #[test]
    fn test_matches_git_archive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_DATE", "2023-11-14T17:13:20Z")
                .env("GIT_COMMITTER_DATE", "2023-11-14T17:13:20Z")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.org")
                .env("GIT_COMMITTER_EMAIL", "t@example.org")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            out.stdout
        };

        git(&["init", "-q", "."]);
        std::fs::write(path.join("README.md"), "hi\n").unwrap();
        std::fs::write(path.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            path.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("README.md", path.join("link.md")).unwrap();
        std::fs::create_dir(path.join("sub")).unwrap();
        std::fs::write(path.join("sub/a.txt"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "t"]);

        let sha = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        let sha = sha.trim();
        let mtime: u64 = String::from_utf8(git(&["log", "-1", "--format=%ct"]))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let theirs = git(&["archive", "--format=tar", "--prefix=demo-main/", "HEAD"]);

        let ours = build_tar(&fixture(), "demo-main/", sha, mtime).unwrap();
        assert_eq!(
            ours.len(),
            theirs.len(),
            "archive length differs from git's ({} vs {})",
            ours.len(),
            theirs.len()
        );
        assert!(
            ours == theirs,
            "archive bytes differ from `git archive`; ours: {:?}",
            entries_of(&ours)
        );
    }

    /// Streaming the tar out in flush-sized pieces must produce exactly the
    /// archive [`build_tar`] produces in one go — the padding in particular,
    /// which is a property of the whole archive's length rather than of the
    /// last piece, and so is the part a chunked writer would get wrong.
    ///
    /// Compression isn't covered here at all — that is the caller's, and in
    /// webgit the browser's own encoder. What is covered is everything fed
    /// *into* it.
    #[test]
    fn test_streamed_tar_matches_whole_tar() {
        let prefix = "demo-main/";
        let commit = "c".repeat(40);
        let mtime = 1_700_000_000;
        let whole = build_tar(&fixture(), prefix, &commit, mtime).unwrap();

        // What a streaming caller writes, taking the tar after every entry so
        // that each one lands in its own chunk.
        let mut writer = TarWriter::new(prefix, &commit, mtime).unwrap();
        let mut streamed = Vec::new();
        for entry in fixture() {
            writer.append(&entry).unwrap();
            streamed.append(&mut writer.take());
        }
        streamed.extend_from_slice(&writer.finish().unwrap());

        assert_eq!(
            streamed, whole,
            "streamed archive differs from the whole one"
        );
        assert_eq!(
            streamed.len() % RECORD_SIZE,
            0,
            "a streamed archive is still padded to a whole record"
        );
    }
}
