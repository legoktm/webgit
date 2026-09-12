//! The static server the browser talks to.
//!
//! This is Apache httpd, configured the way the README tells a deployment to
//! configure it: `DirectoryIndex` for the repository URLs, and the rewrite that
//! puts the app shell behind cgit's path URLs. The suite therefore exercises
//! the recommended deployment rather than an approximation of it.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The port httpd listens on.
const PORT: u16 = 8000;

const MODULE_DIR: &str = "/usr/lib64/httpd/modules";
const MODULES: &[(&str, &str)] = &[
    ("mpm_event_module", "mod_mpm_event.so"),
    ("unixd_module", "mod_unixd.so"),
    ("authz_core_module", "mod_authz_core.so"),
    ("dir_module", "mod_dir.so"),
    ("mime_module", "mod_mime.so"),
    ("rewrite_module", "mod_rewrite.so"),
];

pub struct Server {
    child: Child,
    dir: PathBuf,
}

impl Server {
    pub fn start(webroot: &Path) -> Result<Self> {
        let bin = std::env::var("HTTPD").unwrap_or_else(|_| "httpd".to_string());
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("httpd");
        std::fs::create_dir_all(&dir).context("failed to create the httpd state directory")?;
        let conf = dir.join("httpd.conf");
        std::fs::write(&conf, config(&dir, webroot))?;

        let child = Command::new(&bin)
            .arg("-f")
            .arg(&conf)
            .arg("-DFOREGROUND")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn `{bin}` — is httpd installed?"))?;

        let server = Server { child, dir };
        wait_for_port(PORT, Duration::from_secs(10))
            .with_context(|| format!("httpd never accepted a connection.{}", server.error_log()))?;
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

    /// httpd's own diagnosis of a failed start, for the error that reports it.
    fn error_log(&self) -> String {
        match std::fs::read_to_string(self.dir.join("error.log")) {
            Ok(log) if !log.trim().is_empty() => format!(" Its error log says:\n{}", log.trim()),
            _ => String::new(),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(dir: &Path, webroot: &Path) -> String {
    let load = MODULES
        .iter()
        .map(|(name, file)| format!("LoadModule {name} {MODULE_DIR}/{file}\n"))
        .collect::<String>();
    let (dir, webroot) = (dir.display(), webroot.display());

    format!(
        r#"ServerRoot "{dir}"
ServerName 127.0.0.1
Listen 127.0.0.1:{PORT}
PidFile "{dir}/httpd.pid"
DefaultRuntimeDir "{dir}"
ErrorLog "{dir}/error.log"
LogLevel warn
TypesConfig /etc/mime.types
{load}
DocumentRoot "{webroot}"
DirectoryIndex index.html
<Directory "{webroot}">
    Options FollowSymLinks
    Require all granted
</Directory>

# The README's rule, with the fixtures' prefix in place of public/mirrors.
RewriteEngine On
RewriteCond %{{REQUEST_URI}} ^/repos/[^/]+\.git/((commits?|log|src|tree)(/.*)?)?$
RewriteRule ^ "{webroot}/index.html" [L]
"#
    )
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
