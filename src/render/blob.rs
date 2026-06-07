use git_async::object::ObjectId;
use tera::{Context, Tera};

pub(crate) fn render_blob(
    tera: &Tera,
    blob_id: ObjectId,
    data: &[u8],
    output: &web_sys::Element,
) -> Result<(), String> {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = match lines.as_slice() {
        [rest @ .., ""] => rest.to_vec(),
        other => other.to_vec(),
    };
    let mut ctx = Context::new();
    ctx.insert("blob_id", &format!("{}", blob_id));
    ctx.insert("lines", &lines);
    let html = tera.render("blob.html", &ctx).map_err(|e| format!("Template error: {e}"))?;
    output.set_inner_html(&html);
    Ok(())
}
