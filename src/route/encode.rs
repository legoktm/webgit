//! Percent-encoding for the values a route interpolates.

/// Percent-encode one route component: a ref name, or a single path segment.
///
/// Only what would otherwise read as route syntax is escaped — `%` (the escape
/// itself, so that encoding round-trips), `#` and `?` (which would end the
/// fragment or open a query), `&` (which separates query parameters), `/` (the
/// path separator) and space plus control characters, which a browser would
/// rewrite in the address bar regardless. Everything else, non-ASCII included,
/// stays legible: [`decode_component`] accepts whatever additional escaping the
/// browser applies on its own.
pub(crate) fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '#' | '?' | '&' | '/' | ' ') || c.is_control() {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Decode a percent-encoded route component.
///
/// Escapes are resolved to bytes and the result decoded as UTF-8, so a
/// multi-byte character split across several `%XX` (how a browser encodes
/// non-ASCII in `location.hash`) is reassembled. A `%` that doesn't begin a
/// valid escape is passed through as itself rather than rejected — the hash is
/// user-editable, and a literal `%` in it should still name the obvious thing.
pub(crate) fn decode_component(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode a slash-separated path, one component at a time, so the separators
/// survive as separators while anything inside a component that looks like one
/// is escaped.
pub(crate) fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Reverse of [`encode_path`].
pub(super) fn decode_path(path: &str) -> String {
    path.split('/')
        .map(decode_component)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_component_escapes_only_route_syntax() {
        // Ordinary names are left exactly as they are, so URLs stay readable.
        assert_eq!(encode_component("main"), "main");
        assert_eq!(encode_component("v1.0.0"), "v1.0.0");
        assert_eq!(encode_component("foo(1)+bar,baz=qux"), "foo(1)+bar,baz=qux");
        // The characters that carry meaning in the route grammar.
        assert_eq!(encode_component("a%b"), "a%25b");
        assert_eq!(encode_component("a#b"), "a%23b");
        assert_eq!(encode_component("a?b"), "a%3Fb");
        assert_eq!(encode_component("a&b"), "a%26b");
        assert_eq!(encode_component("a/b"), "a%2Fb");
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("a\tb"), "a%09b");
        // Non-ASCII stays legible; the browser escapes it if it needs to, and
        // the decoder accepts either form.
        assert_eq!(encode_component("café"), "café");
    }

    #[test]
    fn test_decode_component() {
        assert_eq!(decode_component("main"), "main");
        assert_eq!(decode_component("a%2Fb"), "a/b");
        assert_eq!(decode_component("a%3fb"), "a?b", "lowercase hex too");
        // Multi-byte UTF-8 split across escapes, as a browser writes it.
        assert_eq!(decode_component("caf%C3%A9"), "café");
        // A '%' that doesn't begin a valid escape is itself, not an error.
        assert_eq!(decode_component("100%"), "100%");
        assert_eq!(decode_component("50%off"), "50%off");
        assert_eq!(decode_component("%zz"), "%zz");
        assert_eq!(decode_component("%2"), "%2");
    }

    #[test]
    fn test_encode_path_keeps_separators() {
        // Separators survive; a '/' *inside* a component would not.
        assert_eq!(encode_path("src/render/mod.rs"), "src/render/mod.rs");
        assert_eq!(encode_path("docs/a?b.md"), "docs/a%3Fb.md");
        assert_eq!(decode_path("docs/a%3Fb.md"), "docs/a?b.md");
        assert_eq!(encode_path(""), "");
    }
}
