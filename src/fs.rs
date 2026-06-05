use crate::fetch::{fetch_bytes, fetch_text};
use git_async::file_system::{DirEntry, Directory, File, FileSystem, FileSystemError, Offset};
use wasm_bindgen::JsCast;
use web_sys::{DomParser, SupportedType};

#[allow(dead_code)]
pub(crate) struct HttpFile {
    url: String,
    /// Fetched eagerly by `open_file` so that a 404 surfaces as
    /// `FileSystemError::NotFound` during `open_file`, which is when
    /// `git-async` checks for it (to fall back to `packed-refs`).
    pub(crate) data: Vec<u8>,
}

impl File for HttpFile {
    async fn read_all(&mut self) -> Result<Vec<u8>, FileSystemError> {
        Ok(self.data.clone())
    }

    async fn read_segment(
        &mut self,
        offset: Offset,
        dest: &mut [u8],
    ) -> Result<usize, FileSystemError> {
        let start = offset.0 as usize;
        if start >= self.data.len() {
            return Ok(0);
        }
        let available = &self.data[start..];
        let n = dest.len().min(available.len());
        dest[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }
}

#[derive(Clone)]
pub(crate) struct HttpDirectory {
    pub(crate) base_url: String,
}

impl HttpDirectory {
    pub(crate) fn new(base_url: String) -> Self {
        Self { base_url }
    }
}

impl Directory<HttpFile> for HttpDirectory {
    async fn open_subdir(&self, name: &[u8]) -> Result<Self, FileSystemError> {
        let name_str = std::str::from_utf8(name)
            .map_err(|e| FileSystemError::Other(Box::new(e.to_string())))?;
        Ok(HttpDirectory {
            base_url: format!("{}/{}", self.base_url, name_str),
        })
    }

    async fn list_dir(&self) -> Result<Vec<DirEntry>, FileSystemError> {
        let html = fetch_text(&self.base_url).await?;

        // Parse the HTML in a sandboxed document so we can use querySelectorAll.
        let parser = DomParser::new()
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
        let doc = parser
            .parse_from_string(&html, SupportedType::TextHtml)
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;

        // Normalise base_url: strip trailing slash for prefix comparisons.
        let base = self.base_url.trim_end_matches('/');

        // Extract the path portion of base_url (e.g. "/objects/pack" from
        // "http://host/objects/pack") so we can accept miniserve-style
        // absolute hrefs like "/objects/pack/pack-abc.idx".
        let base_path: &str = base
            .find("://")
            .and_then(|i| {
                let after = &base[i + 3..];
                after.find('/').map(|j| &after[j..])
            })
            .unwrap_or("");

        let links = doc
            .query_selector_all("td a")
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;

        let mut entries = Vec::new();
        for i in 0..links.length() {
            let node = links.get(i).unwrap();
            let el: web_sys::Element = node.dyn_into().unwrap();

            // `href` on an anchor in a parsed document is the raw attribute value,
            // not an absolutised URL, so we can treat it directly.
            let href = match el.get_attribute("href") {
                Some(h) => h,
                None => continue,
            };

            // Reject query strings (e.g. "?C=N&O=D" sorting links).
            if href.contains('?') {
                continue;
            }

            // Accept only:
            //   - relative links without a leading slash  (e.g. "objects/")
            //   - absolute links under the same base URL  (e.g. "http://.../repo.git/objects/")
            // Reject parent-directory links ("../"), anchors, and off-origin links.
            let name: &str = if href.starts_with("http://") || href.starts_with("https://") {
                match href.strip_prefix(base) {
                    // must be immediately after the base, with just a '/' separator
                    Some(rest) => rest.trim_matches('/'),
                    None => continue,
                }
            } else if href.starts_with('/') {
                // Absolute path (e.g. miniserve) — strip the base path prefix.
                match href.strip_prefix(base_path) {
                    Some(rest) if !rest.is_empty() => rest.trim_matches('/'),
                    _ => continue,
                }
            } else if href.starts_with('.') || href.contains(':') {
                // relative-dot or scheme -- skip
                continue;
            } else {
                href.trim_end_matches('/')
            };

            if name.is_empty() {
                continue;
            }

            let entry = if href.ends_with('/') {
                web_sys::console::log_1(
                    &format!("list_dir {}: dir  {}", self.base_url, name).into(),
                );
                DirEntry::Directory(name.as_bytes().to_vec())
            } else {
                web_sys::console::log_1(
                    &format!("list_dir {}: file {}", self.base_url, name).into(),
                );
                DirEntry::File(name.as_bytes().to_vec())
            };
            entries.push(entry);
        }

        web_sys::console::log_1(
            &format!(
                "list_dir {}: {} entries total",
                self.base_url,
                entries.len()
            )
            .into(),
        );
        Ok(entries)
    }

    async fn open_file(&self, name: &[u8]) -> Result<HttpFile, FileSystemError> {
        let name_str = std::str::from_utf8(name)
            .map_err(|e| FileSystemError::Other(Box::new(e.to_string())))?;
        let url = format!("{}/{}", self.base_url, name_str);
        let data = fetch_bytes(&url).await?;
        Ok(HttpFile { url, data })
    }
}

pub(crate) struct HttpFilesystem;

impl FileSystem for HttpFilesystem {
    type Directory = HttpDirectory;
    type File = HttpFile;
}
