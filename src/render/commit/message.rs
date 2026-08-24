//! A commit message split into linkable pieces.
//!
//! Escaping is left to Yew, which encodes text nodes as it renders them, so
//! the message is handed over as segments rather than as trusted HTML.

use yew::prelude::*;

/// One piece of a commit message: either literal text (escaped by Yew when
/// rendered) or a SHA-1 reference that becomes a link to that commit. Splitting
/// the message into segments lets Yew handle escaping natively instead of us
/// hand-building trusted HTML.
#[derive(PartialEq, Clone, Debug)]
pub(super) enum MessageSegment {
    Text(String),
    Sha(String),
}

/// hex digits, i.e. a full SHA-1 or one of git's abbreviated forms.
///
/// All-digit runs are included, so a date ("20250101") or a bug number
/// ("1234567") is hex-shaped enough to be linkified. Whether such a link goes
/// anywhere is left to [`resolve_sha`]: one that names no object reports
/// "unknown SHA" rather than rendering as a commit. The alternative — requiring
/// an `a`-`f` — would silence those, but at the cost of the all-digit
/// abbreviations, which are real references a reader would want to follow.
fn is_sha1(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Flush the current alphanumeric run: emit it as a `Sha` segment if it looks
/// like a commit reference, otherwise fold it into the running text buffer.
fn flush_token(token: &mut String, text: &mut String, segments: &mut Vec<MessageSegment>) {
    if is_sha1(token) {
        if !text.is_empty() {
            segments.push(MessageSegment::Text(std::mem::take(text)));
        }
        segments.push(MessageSegment::Sha(std::mem::take(token)));
    } else {
        text.push_str(token);
        token.clear();
    }
}

/// Split a commit message into text/SHA segments. SHA-1 references become links
/// to the referenced commit; everything else is plain text. Escaping is left to
/// Yew, which encodes text nodes when it renders them.
pub(super) fn linkify_message(message: &str) -> Vec<MessageSegment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut token = String::new();

    for c in message.chars() {
        // Word boundaries are ASCII alphanumerics; anything else ends the
        // current token so e.g. a hash inside "word_abc1234" is not matched.
        if c.is_ascii_alphanumeric() {
            token.push(c);
        } else {
            flush_token(&mut token, &mut text, &mut segments);
            text.push(c);
        }
    }
    flush_token(&mut token, &mut text, &mut segments);
    if !text.is_empty() {
        segments.push(MessageSegment::Text(text));
    }
    segments
}

pub(super) fn message_segment(seg: &MessageSegment) -> Html {
    match seg {
        MessageSegment::Text(t) => html! { { t.clone() } },
        MessageSegment::Sha(s) => {
            let href = format!("#!/commit/{s}");
            html! { <a href={href}>{ s.clone() }</a> }
        }
    }
}
