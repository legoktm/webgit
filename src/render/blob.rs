use git_async::object::ObjectId;
use tera::{Context, Tera};

fn blob_context(blob_id: ObjectId, data: &[u8]) -> Context {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = match lines.as_slice() {
        [rest @ .., ""] => rest.to_vec(),
        other => other.to_vec(),
    };
    let mut ctx = Context::new();
    ctx.insert("blob_id", &format!("{}", blob_id));
    ctx.insert("lines", &lines);
    ctx
}

pub(crate) fn render_blob(
    tera: &Tera,
    blob_id: ObjectId,
    data: &[u8],
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let html = tera.render("blob.html", &blob_context(blob_id, data))?;
    output.set_inner_html(&html);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::init_tera;

    fn render(data: &[u8]) -> String {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        init_tera()
            .render("blob.html", &blob_context(id, data))
            .unwrap()
    }

    #[test]
    fn test_blob_html() {
        insta::assert_snapshot!(render(b"fn main() {\n    println!(\"hello\");\n}\n"));
    }

    #[test]
    fn test_blob_html_escapes_markup() {
        insta::assert_snapshot!(render(b"<script>alert(1)</script> & <b>bold</b>\n"));
    }

    #[test]
    fn test_blob_html_empty() {
        insta::assert_snapshot!(render(b""));
    }

    #[test]
    fn test_blob_no_trailing_newline_keeps_last_line() {
        let with = render(b"one\ntwo\n");
        let without = render(b"one\ntwo");
        assert_eq!(with, without);
    }
}
