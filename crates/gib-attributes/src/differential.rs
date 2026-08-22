//! Differential tests for attribute lookup, against `git check-attr`.
//!
//! A repository is built with attributes files at two levels, and every
//! interesting path in it is resolved twice — once by [`Stack`], once by
//! `git check-attr` — and the answers compared. `check-attr` is the same
//! lookup git's own commands make, so this pins the parsing, the precedence
//! between files, the last-line-wins rule within a file, and the pattern
//! matching all at once.
//!
//! Only files are compared, because that is all `check-attr` models: it takes
//! a path, not a path and a type, so a directory-only pattern like `build/` has
//! nothing to match against there. Directory semantics are pinned instead where
//! they are used, by `gib-archive`'s differential test against `git archive`.
//!
//! The fixture defines a macro and uses it, so that macro lines are still
//! exercised as *parsing*, but no path reached through one is compared: git
//! expands macros and this crate does not, and that gap is asserted on its own
//! in `test_macros_are_not_expanded` rather than smuggled in as an exception
//! here.

use crate::{AttributesFile, Stack, State};
use gib_testkit::TestRepo;
use std::collections::HashMap;

/// The attributes files the fixture repository carries, by directory.
const FILES: &[(&str, &str)] = &[
    (
        "",
        "\
# a comment, and a blank line follow

*.txt export-ignore
*.md text
build/ export-ignore
/anchored.txt -export-ignore
doc/*.txt !export-ignore
\"quoted name.txt\" export-ignore
*.bin mytype=binaryish
deep/**/x.txt export-ignore
[attr]mymacro export-ignore
*.macro mymacro
invalid.md export-ignore bad*name
",
    ),
    (
        "sub",
        "\
*.txt -export-ignore
*.log export-ignore
keep.log -export-ignore
",
    ),
];

/// Every path the two implementations are asked about.
const PATHS: &[&str] = &[
    "a.txt",
    "anchored.txt",
    "README.md",
    "invalid.md",
    "doc/a.txt",
    "doc/deeper/a.txt",
    "build/a.txt",
    "sub/a.txt",
    "sub/b.log",
    "sub/keep.log",
    "sub/deeper/c.log",
    "deep/y/x.txt",
    "deep/x.txt",
    "quoted name.txt",
    "thing.bin",
    "plain",
];

/// The attributes asked about, chosen to cover all four kinds of answer.
const ATTRS: &[&str] = &["export-ignore", "text", "mytype"];

/// Build the fixture on disk and return the stack for each directory that has
/// paths under it.
fn stacks() -> HashMap<&'static str, Stack> {
    let mut stacks = HashMap::new();
    let root = Stack::new().push("", AttributesFile::parse(FILES[0].1.as_bytes()));
    let sub = root.push("sub", AttributesFile::parse(FILES[1].1.as_bytes()));
    stacks.insert("", root);
    stacks.insert("sub", sub);
    stacks
}

/// The stack that applies to `path`: the deepest one whose directory contains
/// it. The fixture only has two, which is enough to test that the inner one
/// wins and that the outer one is still reached.
fn stack_for<'a>(stacks: &'a HashMap<&'static str, Stack>, path: &str) -> &'a Stack {
    if path.starts_with("sub/") {
        &stacks["sub"]
    } else {
        &stacks[""]
    }
}

/// Ask git what it thinks, as `path -> attr -> value` in `check-attr`'s own
/// words (`set`, `unset`, `unspecified`, or the value).
fn git_answers(repo: &TestRepo) -> HashMap<(String, String), String> {
    let mut args: Vec<String> = vec!["check-attr".into(), "-z".into()];
    args.extend(ATTRS.iter().map(|a| a.to_string()));
    args.push("--".into());
    args.extend(PATHS.iter().map(|p| p.to_string()));

    let out = repo.run_git(args).expect("git check-attr runs");
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    let mut answers = HashMap::new();
    for triple in fields.chunks(3) {
        // The output ends with a trailing NUL, so the last chunk is short.
        let [path, attr, value] = triple else {
            continue;
        };
        answers.insert(
            (
                String::from_utf8_lossy(path).into_owned(),
                String::from_utf8_lossy(attr).into_owned(),
            ),
            String::from_utf8_lossy(value).into_owned(),
        );
    }
    answers
}

/// Our answer, in the same words.
fn describe(state: State<'_>) -> String {
    match state {
        State::Set => "set".to_string(),
        State::Unset => "unset".to_string(),
        State::Unspecified => "unspecified".to_string(),
        State::Value(v) => v.to_string(),
    }
}

#[test]
fn test_matches_git_check_attr() {
    let repo = TestRepo::new().expect("a repository");
    for (dir, contents) in FILES {
        let path = repo.location.path().join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(".gitattributes"), contents).unwrap();
    }

    let git = git_answers(&repo);
    let stacks = stacks();

    let mut disagreements = Vec::new();
    for path in PATHS {
        for attr in ATTRS {
            let ours = describe(stack_for(&stacks, path).check(path, false, attr));
            let theirs = git
                .get(&(path.to_string(), attr.to_string()))
                .unwrap_or_else(|| panic!("git said nothing about {attr} for {path}"));
            if &ours != theirs {
                disagreements.push(format!("{path}: {attr}: ours {ours}, git {theirs}"));
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} of {} lookups disagree with git:\n{}",
        disagreements.len(),
        PATHS.len() * ATTRS.len(),
        disagreements.join("\n")
    );
}
