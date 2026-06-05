use git_async::object::ObjectId;
use tera::{Context, Tera};

pub(crate) fn render_blob(tera: &Tera, blob_id: ObjectId, data: &[u8], output: &web_sys::Element) {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = match lines.as_slice() {
        [rest @ .., ""] => rest.to_vec(),
        other => other.to_vec(),
    };
    let mut ctx = Context::new();
    ctx.insert("blob_id", &format!("{}", blob_id));
    ctx.insert("lines", &lines);
    match tera.render("blob.html", &ctx) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}
