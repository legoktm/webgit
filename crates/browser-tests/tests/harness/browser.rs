//! geckodriver supervision and headless Firefox sessions.
//!
//! One geckodriver and one Firefox per test. That is not the fastest
//! arrangement, but it is the only one that gives each test an empty
//! IndexedDB — which matters, because webgit caches git objects there keyed by
//! repository URL, and a leaked cache would quietly invalidate exactly the
//! tests that care about fetching.
//!
//! geckodriver also serves a single session at a time, so the suite runs with
//! `--test-threads=1`; see scripts/browser-tests.sh.

use anyhow::{Context, Result};
use fantoccini::{Client, ClientBuilder};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::server::wait_for_port;

/// The port geckodriver listens on
const PORT: u16 = 4444;

pub struct Driver {
    child: Child,
}

impl Driver {
    pub fn start() -> Result<Self> {
        let port = PORT;
        let bin = std::env::var("GECKODRIVER").unwrap_or_else(|_| "geckodriver".to_string());

        let child = Command::new(&bin)
            .arg("--port")
            .arg(port.to_string())
            // Bind explicitly to loopback; geckodriver's default host is
            // already localhost, but being explicit keeps it off any other
            // interface the container might have.
            .arg("--host")
            .arg("127.0.0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn `{bin}` — is geckodriver installed?"))?;

        let driver = Driver { child };
        wait_for_port(port, Duration::from_secs(20)).context("geckodriver never came up")?;
        Ok(driver)
    }

    /// Open a headless Firefox session with a fresh profile.
    ///
    /// `download_dir` is wired into the profile so the snapshot test can assert
    /// on a file appearing rather than trying to observe the download UI.
    pub async fn session(&self, download_dir: &Path) -> Result<Client> {
        let mut caps = Map::new();
        caps.insert(
            "moz:firefoxOptions".to_string(),
            json!({
                "args": ["-headless"],
                "prefs": {
                    // 2 = use the configured directory rather than ~/Downloads.
                    "browser.download.folderList": 2,
                    "browser.download.dir": download_dir.to_string_lossy(),
                    // Save these straight to disk instead of opening a dialog
                    // that a headless session can never answer.
                    "browser.download.useDownloadDir": true,
                    "browser.helperApps.neverAsk.saveToDisk":
                        "application/x-tar,application/gzip,application/octet-stream",
                    // Keep a first-run profile from making requests of its own,
                    // which would otherwise show up in the resource timings the
                    // caching test measures.
                    "browser.shell.checkDefaultBrowser": false,
                    "network.dns.disablePrefetch": true,
                    "network.prefetch-next": false,
                    "toolkit.telemetry.enabled": false,
                    "datareporting.healthreport.uploadEnabled": false,
                }
            }),
        );
        caps.insert("pageLoadStrategy".to_string(), Value::from("normal"));

        let client = ClientBuilder::native()
            .capabilities(caps)
            .connect(&format!("http://127.0.0.1:{PORT}"))
            .await
            .context("failed to start a Firefox session")?;
        Ok(client)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Killing geckodriver takes its Firefox child with it, which is what
        // keeps a panicking test from leaving a browser behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
