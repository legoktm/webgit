//! Test harness: fixtures on disk, miniserve in front of them, and a headless
//! Firefox driving the real `dist/` build.

pub mod browser;
pub mod fixtures;
pub mod server;

use anyhow::{Context, Result, bail};
use fantoccini::{Client, Locator, elements::Element};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub use fixtures::{Fixtures, RepoFixture};

/// How long to wait for a route's content to appear. Generous: a cold first
/// load has to fetch and inflate git objects before it can render anything.
const SETTLE: Duration = Duration::from_secs(30);

pub struct Harness {
    pub fixtures: &'static Fixtures,
    pub server: server::Server,
    pub client: Client,
    pub downloads: PathBuf,
    /// Declared last so it is dropped last: killing geckodriver tears down
    /// Firefox, and doing that before the client is dropped is harmless but
    /// noisier in the logs.
    _driver: browser::Driver,
}

impl Harness {
    pub async fn start() -> Result<Self> {
        let fixtures = fixtures::get()?;
        let server = server::Server::start(&fixtures.webroot)?;
        let driver = browser::Driver::start()?;

        let downloads = unique_dir("downloads")?;
        let client = driver.session(&downloads).await?;

        Ok(Harness {
            fixtures,
            server,
            client,
            downloads,
            _driver: driver,
        })
    }

    /// Navigate to a route within a fixture repository, e.g. `"#!/log"`.
    pub async fn open(&self, repo: &RepoFixture, route: &str) -> Result<()> {
        let url = self.server.url(&format!("{}{}", repo.url_path(), route));
        self.client.goto(&url).await?;
        Ok(())
    }

    /// Navigate to the repository index — the URL that names no repository.
    pub async fn open_index(&self) -> Result<()> {
        self.client.goto(&self.server.url("/")).await?;
        Ok(())
    }

    /// Wait for an element to exist, then return it.
    pub async fn wait_for(&self, css: &str) -> Result<Element> {
        self.client
            .wait()
            .at_most(SETTLE)
            .for_element(Locator::Css(css))
            .await
            .with_context(|| {
                format!("no element matched `{css}` within {SETTLE:?} — page did not render")
            })
    }

    /// Wait for an element and return its rendered text.
    pub async fn text_of(&self, css: &str) -> Result<String> {
        Ok(self.wait_for(css).await?.text().await?)
    }

    /// The rendered text of every element matching `css`, in document order.
    pub async fn texts_of(&self, css: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for el in self.client.find_all(Locator::Css(css)).await? {
            out.push(el.text().await?);
        }
        Ok(out)
    }

    /// The whole content area as text. Useful for "does this page mention X"
    /// assertions where the exact element is not the point.
    pub async fn content_text(&self) -> Result<String> {
        self.text_of("#content").await
    }

    /// The app renders load failures as `<p class="msg error">`. Nothing in
    /// this suite should ever produce one, so every test checks.
    pub async fn assert_no_error(&self) -> Result<()> {
        let errors = self.texts_of("#content .msg.error").await?;
        if !errors.is_empty() {
            bail!("page rendered an error: {}", errors.join(" | "));
        }
        Ok(())
    }

    /// The current location hash, as the browser sees it.
    pub async fn hash(&self) -> Result<String> {
        let url = self.client.current_url().await?;
        Ok(url.fragment().map(|f| format!("#{f}")).unwrap_or_default())
    }

    /// Every URL the page has fetched, from the Resource Timing API.
    ///
    /// This is how the caching test measures network activity. Resource
    /// timings are per-document and reset on reload, which is exactly the
    /// boundary being measured — so a first load and a reload can be compared
    /// without the server needing to keep a request log.
    ///
    /// WebDriver script execution is not subject to the page's CSP, so this
    /// works despite `script-src 'self'`.
    pub async fn fetched_urls(&self) -> Result<Vec<String>> {
        let value = self
            .client
            .execute(
                "return performance.getEntriesByType('resource').map(e => e.name);",
                vec![],
            )
            .await?;
        let urls = value
            .as_array()
            .context("resource timings were not an array")?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        Ok(urls)
    }

    /// Requests the page made for git objects, which is what IndexedDB caching
    /// is supposed to eliminate on a second visit. Filters out the app's own
    /// assets, which are served by the browser cache on their own terms.
    pub async fn fetched_object_urls(&self, repo: &RepoFixture) -> Result<Vec<String>> {
        let prefix = repo.url_path();
        Ok(self
            .fetched_urls()
            .await?
            .into_iter()
            .filter(|u| u.contains(&prefix) && !u.ends_with("index.html"))
            .filter(|u| u.contains("/objects/") || u.contains("/info/") || u.ends_with("/HEAD"))
            .collect())
    }

    /// Close the session cleanly. Not required — dropping the harness kills
    /// geckodriver, which takes Firefox with it — but it keeps a passing run
    /// free of "connection reset" noise in geckodriver's output.
    pub async fn finish(self) -> Result<()> {
        self.client.clone().close().await?;
        Ok(())
    }
}

/// A fresh directory under the target dir, unique per call within this process.
fn unique_dir(prefix: &str) -> Result<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{prefix}-{n}"));
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    Ok(path)
}
