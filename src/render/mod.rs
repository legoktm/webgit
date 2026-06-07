use git_async::object::Commit;
use serde::Serialize;
use tera::{Kwargs, State, Tera, TeraResult, Value};

pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod log;
pub(crate) mod refs_all;
pub(crate) mod refs_heads;
pub(crate) mod refs_tags;
pub(crate) mod summary;
pub(crate) mod tag;
pub(crate) mod tree;

pub(crate) fn init_tera() -> Tera {
    let mut tera = Tera::default();
    tera.register_filter("age_string", age_string);
    tera.add_raw_templates(vec![
        ("blob.html", include_str!("../templates/blob.html")),
        (
            "refs_heads.html",
            include_str!("../templates/refs_heads.html"),
        ),
        (
            "refs_tags.html",
            include_str!("../templates/refs_tags.html"),
        ),
        ("refs_all.html", include_str!("../templates/refs_all.html")),
        ("summary.html", include_str!("../templates/summary.html")),
        ("tree.html", include_str!("../templates/tree.html")),
        ("tag.html", include_str!("../templates/tag.html")),
        ("commit.html", include_str!("../templates/commit.html")),
        ("commits.html", include_str!("../templates/commits.html")),
        ("log.html", include_str!("../templates/log.html")),
    ])
    .unwrap();
    tera
}

#[derive(Serialize)]
struct RefRow {
    name: String,
    short_hash: String,
    message: String,
    author: String,
    age: u64,
}

#[derive(Serialize)]
struct CommitRow {
    hash: String,
    short_hash: String,
    message: String,
    author: String,
    age: u64,
}

fn age(dt: &chrono::DateTime<chrono::FixedOffset>) -> u64 {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp_millis() as f64;
    ((now_ms - then_ms) / 1000.0).max(0.0) as u64
}

fn age_string(value: Value, _: Kwargs, _: &State) -> TeraResult<Value> {
    let secs = value.as_u128().unwrap() as u64;
    let formatted = match secs {
        s if s < 90 => format!("{} seconds", s),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 36 * 3600 => format!("{} hours", s / 3600),
        s if s < 14 * 86400 => format!("{} days", s / 86400),
        s if s < 8 * 7 * 86400 => format!("{} weeks", s / (7 * 86400)),
        s if s < 24 * 30 * 86400 => format!("{} months", s / (30 * 86400)),
        s => format!("{} years", s / (365 * 86400)),
    };
    Ok(Value::from(formatted))
}

fn commit_first_line(c: &Commit) -> String {
    String::from_utf8_lossy(c.message())
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    let hash = format!("{}", c.id());
    RefRow {
        name,
        short_hash: hash[..8].to_string(),
        message: commit_first_line(c),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: age(&c.author_date()),
    }
}
