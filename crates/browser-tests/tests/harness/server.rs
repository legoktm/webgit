//! The static server the browser talks to.
//!
//! This is `miniserve`, not something written here, because the routing webgit
//! needs can be expressed as filesystem layout instead of rewrite rules — see
//! `fixtures::install`. What it must do correctly is `Range`: webgit's whole
//! object-fetching design rests on byte ranges into packfiles, and
//! `classify()` in `src/fetch.rs` deliberately accepts a 200 answer to a range
//! request and slices client-side. A server that ignores `Range` therefore
//! produces a suite that passes while never exercising the 206 path at all.
//! `tests/browser.rs` asserts a real 206 before anything else runs.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The port miniserve listens on.
const PORT: u16 = 8000;

pub struct Server {
    child: Child,
}

impl Server {
    pub fn start(webroot: &std::path::Path) -> Result<Self> {
        let port = PORT;
        let bin = std::env::var("MINISERVE").unwrap_or_else(|_| "miniserve".to_string());

        let child = Command::new(&bin)
            .arg("--index")
            .arg("index.html")
            .arg("--interfaces")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg(webroot)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn `{bin}` — is miniserve installed?"))?;

        let server = Server { child };
        wait_for_port(port, Duration::from_secs(10))
            .context("miniserve never accepted a connection")?;
        Ok(server)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{PORT}")
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    pub fn port(&self) -> u16 {
        PORT
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A parsed HTTP response. Deliberately minimal: the only thing tested through
/// it is the server's own `Range` behaviour, which needs a status line, a
/// couple of headers, and a body.
pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Issue a GET against the harness server, optionally with a `Range` header.
///
/// Raw sockets rather than an HTTP client crate: this is one request against
/// localhost, and the alternative is pulling a full client into the dependency
/// tree to make it.
pub fn get(port: u16, path: &str, range: Option<&str>) -> Result<Response> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(range) = range {
        request.push_str(&format!("Range: {range}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no header/body boundary in response")?;
    let head = std::str::from_utf8(&raw[..split])?;
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().context("empty response")?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .context("malformed status line")?
        .parse()?;

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();

    Ok(Response {
        status,
        headers,
        body,
    })
}

pub fn wait_for_port(port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("nothing listening on 127.0.0.1:{port} after {timeout:?}")
}
