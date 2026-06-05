use tera::Tera;

pub(crate) mod blob;
pub(crate) mod summary;
pub(crate) mod tree;

pub(crate) fn init_tera() -> Tera {
    let mut tera = Tera::default();
    tera.add_raw_templates(vec![
        ("blob.html", include_str!("../templates/blob.html")),
        ("summary.html", include_str!("../templates/summary.html")),
        ("tree.html", include_str!("../templates/tree.html")),
    ])
    .unwrap();
    tera
}
