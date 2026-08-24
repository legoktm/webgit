//! The mbox-style document a patch is wrapped in: the `From `/`From:`/`Date:`/
//! `Subject:` header block, the message body, and the mail signature at the
//! foot that `git am` stops reading at.

use crate::diff::FileDiff;
use crate::stat::diffstat;
use gib_object::Commit;

/// Everything a patch's header block says about the commit, in the form it
/// will be written in.
///
/// Kept as rendered strings rather than as a borrowed [`Commit`] so that a
/// caller can hold it for as long as the view it belongs to — building the
/// patch itself is deferred until someone asks for the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchMeta {
    /// The commit's full hash, which heads the patch.
    pub hash: String,
    /// The author's name, for the `From:` header.
    pub author_name: String,
    /// The author's email address.
    pub author_email: String,
    /// The author date, already in RFC 2822 form.
    pub date: String,
    /// The full commit message, subject line included.
    pub message: String,
}

impl PatchMeta {
    /// Take the header fields from a commit. The author, not the committer, is
    /// what a patch carries: applying it makes the reader the committer.
    #[must_use]
    pub fn from_commit(commit: &Commit) -> Self {
        Self {
            hash: format!("{}", commit.id()),
            author_name: String::from_utf8_lossy(commit.author_name()).into_owned(),
            author_email: String::from_utf8_lossy(commit.author_email()).into_owned(),
            // The only dates RFC 2822 cannot write are ones outside its
            // four-digit years, and no plausible commit has one.
            date: jiff::fmt::rfc2822::to_string(commit.author_date()).unwrap_or_default(),
            message: String::from_utf8_lossy(commit.message()).into_owned(),
        }
    }
}

/// Render the commit and its diff as a patch file.
///
/// `generator` names what produced the patch and goes in the mail signature at
/// the foot, where git writes its own version.
///
/// The `files` must have been diffed with [`DiffOptions::default`]. A patch is
/// applied, not read: `git apply` matches the bytes it is given, so a diff
/// taken at any other context width — or with whitespace ignored — is a
/// document that looks like a patch and does not apply as one.
#[must_use]
pub fn format_patch<'a>(
    meta: &PatchMeta,
    files: impl IntoIterator<Item = &'a FileDiff>,
    generator: &str,
) -> String {
    // git walks its diff queue in path order, both in the stat and in the diff
    // below it, whatever order the caller found the files in.
    let mut files: Vec<&FileDiff> = files.into_iter().collect();
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let (subject, body) = split_message(&meta.message);

    let mut out = String::new();
    // git's magic mbox date, which marks the file as a formatted patch rather
    // than a real mail; `git am` looks for it and never reads it as a date.
    out.push_str(&format!("From {} Mon Sep 17 00:00:00 2001\n", meta.hash));
    out.push_str(&format!(
        "From: {} <{}>\n",
        rfc2047(&meta.author_name, "From: ".len(), true),
        meta.author_email
    ));
    out.push_str(&format!("Date: {}\n", meta.date));
    if subject.is_ascii() {
        out.push_str(&fold_subject(&subject));
    } else {
        let prefix = "Subject: [PATCH] ";
        out.push_str(&format!(
            "{prefix}{}\n",
            rfc2047(&subject, prefix.len(), false)
        ));
    }
    // The headers are encoded whatever the message says, but the body is sent
    // as raw UTF-8, so it is the message — not the author's name — that decides
    // whether the encoding has to be declared.
    if !subject.is_ascii() || !body.is_ascii() {
        out.push_str(
            "MIME-Version: 1.0\n\
             Content-Type: text/plain; charset=UTF-8\n\
             Content-Transfer-Encoding: 8bit\n",
        );
    }
    out.push('\n');
    out.push_str(&body);
    out.push_str("---\n");
    out.push_str(&diffstat(&files));

    for line in files.iter().flat_map(|f| f.lines.iter()) {
        out.push_str(&line.text);
        out.push('\n');
    }

    // The mail signature git ends a patch with, naming what wrote it; `git am`
    // stops reading here.
    out.push_str(&format!("-- \n{generator}\n\n"));
    out
}

/// Split a commit message into the patch's subject and body.
///
/// The subject is the first paragraph joined into one line, as git does it, so
/// a message whose first two lines run on still yields a single `Subject:`. The
/// body is everything after that paragraph, with the blank line that separated
/// them dropped — the header block writes its own.
fn split_message(message: &str) -> (String, String) {
    let mut lines = message.lines();
    let mut subject = String::new();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if !subject.is_empty() {
            subject.push(' ');
        }
        subject.push_str(line.trim_end());
    }
    let rest: Vec<&str> = lines.collect();
    let mut body = String::new();
    if !rest.is_empty() {
        for line in rest {
            body.push_str(line);
            body.push('\n');
        }
        // A message that ends in blank lines would otherwise push the `---`
        // away from the text; git trims back to one trailing newline.
        while body.ends_with("\n\n") {
            body.pop();
        }
    }
    (subject, body)
}

/// Write the `Subject:` header, folded the way mail headers are: at most 78
/// columns, continuation lines indented by one space, and never mid-word.
fn fold_subject(subject: &str) -> String {
    const LIMIT: usize = 78;
    let mut out = String::from("Subject: [PATCH] ");
    let mut column = out.len();
    let mut first = true;
    for word in subject.split(' ') {
        if !first && column + 1 + word.len() > LIMIT {
            out.push_str("\n ");
            column = 1;
        } else if !first {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
        first = false;
    }
    out.push('\n');
    out
}

/// The longest an encoded header line may be, per RFC 2047.
const MAX_ENCODED_LENGTH: usize = 76;

/// Encode a header value as RFC 2047 `q`-encoded words, as git does for a name
/// or subject that cannot go out as it stands. Anything not a printable ASCII
/// character — and the ones the syntax reserves, plus the space that would
/// otherwise end a word — becomes `=XX` per byte.
///
/// `line_len` is how much of the line the header name has already used, since
/// that is what decides where the value has to be broken: a value too long for
/// one line continues in a second encoded word on the next, indented by a
/// space. Words are only ever broken between characters, never inside one.
///
/// A value that needs no encoding is returned untouched, so this is safe to
/// call on every header.
fn rfc2047(value: &str, line_len: usize, address: bool) -> String {
    // git's rule: only non-ASCII, an embedded newline, or something that would
    // read back as an encoded word forces the encoding. Spaces and `=` on
    // their own are left alone.
    if value.is_ascii() && !value.contains('\n') && !value.contains("=?") {
        return value.to_string();
    }
    const CHARSET: &str = "UTF-8";
    // `=?`, `?q?` and the charset between them.
    let word_overhead = CHARSET.len() + 5;

    let mut out = format!("=?{CHARSET}?q?");
    let mut line_len = line_len + word_overhead;
    for c in value.chars() {
        let mut encoded = String::new();
        let mut buf = [0u8; 4];
        for &byte in c.encode_utf8(&mut buf).as_bytes() {
            if is_rfc2047_special(byte, address) {
                encoded.push_str(&format!("={byte:02X}"));
            } else {
                encoded.push(byte as char);
            }
        }
        // The 2 is the `?=` that will close the word at the end of the line.
        if line_len + encoded.len() + 2 > MAX_ENCODED_LENGTH {
            out.push_str(&format!("?=\n =?{CHARSET}?q?"));
            line_len = word_overhead + 1;
        }
        out.push_str(&encoded);
        line_len += encoded.len();
    }
    out.push_str("?=");
    out
}

/// Whether a byte has to be written as `=XX` inside an encoded word, by RFC
/// 2047's rules as git reads them: never a non-printable or non-ASCII byte,
/// never whitespace, and never the three the syntax reserves.
///
/// A display name is held to the stricter rule for a mail phrase, where only
/// letters, digits and a handful of punctuation may stand for themselves.
fn is_rfc2047_special(byte: u8, address: bool) -> bool {
    if !byte.is_ascii() || !(byte.is_ascii_graphic() || byte == b' ') {
        return true;
    }
    if byte.is_ascii_whitespace() || matches!(byte, b'=' | b'?' | b'_') {
        return true;
    }
    address && !(byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'*' | b'+' | b'-' | b'/'))
}
