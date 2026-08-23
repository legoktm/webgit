//! Differential tests for contact mapping, against `git check-mailmap`.
//!
//! A repository is built with a `.mailmap` at its root, and every contact in
//! the corpus is mapped twice — once by [`Mailmap`], once by
//! `git check-mailmap` — and the answers compared. `check-mailmap` is a thin
//! wrapper over the `map_user` that `git log` itself calls, so this pins the
//! parsing of every line shape, which half of a contact each one replaces, the
//! precedence between an email-only entry and a name-keyed one, and the case
//! folding of both, all at once.

use crate::Mailmap;
use gib_testkit::TestRepo;

/// The fixture `.mailmap`, one stanza per behaviour the corpus below probes.
const MAILMAP: &str = "\
# a comment, and a blank line follow

Proper One <one@proper.example> <one@commit.example>
Proper Two <two@commit.example>
<three@proper.example> <three@commit.example>
Proper Four <four@proper.example> Commit Four <four@commit.example>
Fallback Four <fallback@proper.example> <four@commit.example>
Proper Five <five@proper.example> Commit Five <FIVE@Commit.Example>
\tProper Six \t<six@proper.example> <six@commit.example>
Proper Seven <seven@proper.example> <seven@commit.example> trailing junk
Combined Eight <eight@commit.example>
<eight@proper.example> <eight@commit.example>
Replaced Nine <nine@first.example> Commit Nine <nine@commit.example>
<nine@second.example> Commit Nine <nine@commit.example>
<ten@commit.example>
 # Proper Eleven <eleven@commit.example>
Broken Twelve <twelve@commit.example
<> <thirteen@commit.example>
";

/// Every contact both implementations are asked about, as the (name, email)
/// pair a commit carries.
const CONTACTS: &[(&str, &str)] = &[
    ("Commit One", "one@commit.example"),
    ("Commit Two", "two@commit.example"),
    ("Commit Three", "three@commit.example"),
    // The name-keyed entry, the email-only fallback beside it, and both again
    // in a case that matches neither literally.
    ("Commit Four", "four@commit.example"),
    ("Someone Else", "four@commit.example"),
    ("COMMIT FOUR", "FOUR@COMMIT.EXAMPLE"),
    // The entry itself was written in mixed case.
    ("Commit Five", "five@commit.example"),
    ("Commit Six", "six@commit.example"),
    ("Commit Seven", "seven@commit.example"),
    // Two lines, each supplying one half.
    ("Commit Eight", "eight@commit.example"),
    // Two name-keyed lines for one name: the later replaces the earlier.
    ("Commit Nine", "nine@commit.example"),
    // An entry that supplies neither half.
    ("Commit Ten", "ten@commit.example"),
    // Lines git drops: an indented `#` is a name, an unclosed `<` is nothing,
    // and an empty canonical address is not allowed.
    ("Commit Eleven", "eleven@commit.example"),
    ("Commit Twelve", "twelve@commit.example"),
    ("Commit Thirteen", "thirteen@commit.example"),
    // Nothing in the file mentions this one.
    ("Nobody", "nobody@example.org"),
];

/// A contact in the form `check-mailmap` reads and writes.
fn contact(name: &str, email: &str) -> String {
    format!("{name} <{email}>")
}

/// Ask git to map every contact, in corpus order.
fn git_answers(repo: &TestRepo) -> Vec<String> {
    let mut args = vec!["check-mailmap".to_string(), "--".to_string()];
    args.extend(CONTACTS.iter().map(|(n, e)| contact(n, e)));
    let out = repo.run_git(args).expect("git check-mailmap runs");
    String::from_utf8(out)
        .expect("check-mailmap output is UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn test_matches_git_check_mailmap() {
    let repo = TestRepo::new().expect("a repository");
    std::fs::write(repo.location.path().join(".mailmap"), MAILMAP).unwrap();

    let git = git_answers(&repo);
    assert_eq!(
        git.len(),
        CONTACTS.len(),
        "git answered about {} of {} contacts",
        git.len(),
        CONTACTS.len()
    );

    let map = Mailmap::parse(MAILMAP.as_bytes());
    let mut disagreements = Vec::new();
    for ((name, email), theirs) in CONTACTS.iter().zip(&git) {
        let (mapped_name, mapped_email) = map.map(name.as_bytes(), email.as_bytes());
        let ours = contact(
            &String::from_utf8_lossy(mapped_name),
            &String::from_utf8_lossy(mapped_email),
        );
        if &ours != theirs {
            disagreements.push(format!(
                "{}: ours {ours}, git {theirs}",
                contact(name, email)
            ));
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} of {} contacts disagree with git:\n{}",
        disagreements.len(),
        CONTACTS.len(),
        disagreements.join("\n")
    );
}
