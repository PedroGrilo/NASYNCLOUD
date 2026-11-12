//! Local web interface on http://127.0.0.1:PORT: status, file browser,
//! activity and settings. The page is embedded in the binary; the JSON API
//! needs a random token (kept in `ui.json` next to the settings) and only
//! answers to localhost Host headers, against DNS-rebinding tricks.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nasyncloud::config::Config;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::service::{Mode, Service};

const PAGE: &str = include_str!("ui/index.html");
/// The compact panel the desktop app shows from the tray / menu bar.
const PANEL: &str = include_str!("ui/panel.html");
const TOKEN_HEADER: &str = "x-nasyncloud-token";
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Serialize, Deserialize)]
struct UiFile {
    port: u16,
    token: String,
}

pub struct Ui {
    pub port: u16,
    pub token: String,
}

impl Ui {
    /// The token travels in the fragment, which browsers never send anywhere.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/#t={}", self.port, self.token)
    }
}

fn ui_file(cfg_path: &Path) -> PathBuf {
    cfg_path.with_file_name("ui.json")
}

fn read_ui_file(cfg_path: &Path) -> Option<UiFile> {
    serde_json::from_slice(&std::fs::read(ui_file(cfg_path)).ok()?).ok()
}

fn write_ui_file(cfg_path: &Path, file: &UiFile) -> Result<()> {
    let path = ui_file(cfg_path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)?.write_all(&serde_json::to_vec(file)?)?;
    Ok(())
}

/// 256 random bits, from the OS-seeded keys of std's hasher (no extra crate).
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ u64::from(std::process::id());
    (0..4u64)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(seed.rotate_left(16 * i as u32) ^ i);
            format!("{:016x}", h.finish())
        })
        .collect()
}

pub async fn serve(svc: Arc<Service>, port: u16) -> Result<Ui> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await.with_context(|| format!("cannot listen on 127.0.0.1:{port} (is another program using that port?)"))?;
    let port = listener.local_addr()?.port();
    // Keep the token across restarts, so an open tab keeps working.
    let token = read_ui_file(&svc.cfg_path).map(|f| f.token).filter(|t| t.len() >= 32 && t.bytes().all(|b| b.is_ascii_hexdigit())).unwrap_or_else(new_token);
    write_ui_file(&svc.cfg_path, &UiFile { port, token: token.clone() })?;
    let handler = Arc::new(Handler { svc, port, token: token.clone() });
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    log::debug!("interface: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler.handle(req).await) }
                });
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    Ok(Ui { port, token })
}

/// `nasyncloud ui`: open the running instance, or start one.
pub fn open_or_run(cfg_path: &Path) -> Result<()> {
    if let Some(f) = read_ui_file(cfg_path) {
        if ping(f.port, &f.token) {
            let ui = Ui { port: f.port, token: f.token };
            println!("nasyncloud is already running; opening http://127.0.0.1:{}", ui.port);
            if open_path(&ui.url()).is_err() {
                println!("Open {} in your browser.", ui.url());
            }
            return Ok(());
        }
    }
    crate::service::run(cfg_path, Mode::App)
}

fn ping(port: u16, token: &str) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return false };
    rt.block_on(async {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().ok()?;
        let resp = client.get(format!("http://127.0.0.1:{port}/api/ping")).header(TOKEN_HEADER, token).send().await.ok()?;
        Some(resp.status().is_success())
    })
    .unwrap_or(false)
}

/// Opens a URL, folder or file with the desktop's default application.
pub fn open_path(target: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(windows)]
    let mut cmd = if target.starts_with("http") {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("explorer")
    };
    cmd.arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("cannot open {target}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[derive(Deserialize)]
struct TestReq {
    url: String,
    username: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    accept_invalid_certs: bool,
}

#[derive(Deserialize)]
struct SaveReq {
    config: Value,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    store_in_file: bool,
}

#[derive(Deserialize)]
struct PathReq {
    #[serde(default)]
    path: String,
    #[serde(default)]
    on: Option<bool>,
}

type Resp = Response<Full<Bytes>>;

fn respond(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Resp {
    let mut resp = Response::new(Full::new(body.into()));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    resp
}

fn json_resp(status: StatusCode, value: &Value) -> Resp {
    respond(status, "application/json", serde_json::to_vec(value).unwrap_or_default())
}

/// Value of `key` in a query string.
fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| percent_decode_str(&v.replace('+', " ")).decode_utf8_lossy().into_owned())
    })
}

/// Normalises a relative path from the browser; refuses `..` and friends.
fn clean_rel(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if part == "." || part == ".." || part.contains('\0') {
            bail!("invalid path");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// Constant-time comparison.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Handler {
    svc: Arc<Service>,
    port: u16,
    token: String,
}

impl Handler {
    async fn handle(&self, req: Request<Incoming>) -> Resp {
        match self.route(req).await {
            Ok(resp) => resp,
            Err(e) => json_resp(StatusCode::BAD_REQUEST, &json!({ "error": format!("{e:#}") })),
        }
    }

    fn host_ok(&self, req: &Request<Incoming>) -> bool {
        let Some(host) = req.headers().get(HOST).and_then(|h| h.to_str().ok()) else { return false };
        let p = self.port;
        [format!("127.0.0.1:{p}"), format!("localhost:{p}"), format!("[::1]:{p}")].iter().any(|ok| ok.eq_ignore_ascii_case(host))
    }

    async fn route(&self, req: Request<Incoming>) -> Result<Resp> {
        if !self.host_ok(&req) {
            return Ok(respond(StatusCode::MISDIRECTED_REQUEST, "text/plain", "open the interface on http://127.0.0.1"));
        }
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or_default().to_string();
        let page = match path.as_str() {
            "/" | "/index.html" => Some(PAGE),
            "/panel" => Some(PANEL),
            _ => None,
        };
        if let (true, Some(page)) = (method == Method::GET, page) {
            let mut resp = respond(StatusCode::OK, "text/html; charset=utf-8", page);
            resp.headers_mut().insert("content-security-policy", HeaderValue::from_static(CSP));
            resp.headers_mut().insert("x-frame-options", HeaderValue::from_static("DENY"));
            return Ok(resp);
        }
        if !path.starts_with("/api/") {
            return Ok(respond(StatusCode::NOT_FOUND, "text/plain", "not found"));
        }
        let authed = req.headers().get(TOKEN_HEADER).and_then(|v| v.to_str().ok()).is_some_and(|t| same(t.as_bytes(), self.token.as_bytes()));
        if !authed {
            return Ok(json_resp(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthorized" })));
        }
        let body: Value = if method == Method::POST {
            let bytes = Limited::new(req.into_body(), 64 * 1024).collect().await.map_err(|e| anyhow!("request body: {e}"))?.to_bytes();
            if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).context("invalid JSON")?
            }
        } else {
            Value::Null
        };
        let svc = &self.svc;
        let ok = json!({ "ok": true });
        let out = match (method.as_str(), path.as_str()) {
            ("GET", "/api/ping") => ok,
            ("GET", "/api/state") => self.state(),
            ("GET", "/api/log") => {
                let after = param(&query, "after").and_then(|v| v.parse().ok()).unwrap_or(0);
                json!({ "lines": crate::logging::since(after) })
            }
            ("POST", "/api/test") => {
                let r: TestReq = serde_json::from_value(body)?;
                serde_json::to_value(svc.test(r.url, r.username, r.password, r.accept_invalid_certs).await)?
            }
            ("POST", "/api/config") => {
                let r: SaveReq = serde_json::from_value(body)?;
                let cfg: Config = serde_json::from_value(r.config).context("invalid settings")?;
                serde_json::to_value(svc.save_config(cfg, r.password, r.store_in_file).await?)?
            }
            ("POST", "/api/mount") => {
                let s = svc.clone();
                tokio::spawn(async move {
                    let _ = s.start_mount().await;
                });
                ok
            }
            ("POST", "/api/unmount") => {
                let s = svc.clone();
                tokio::spawn(async move { s.stop_mount().await });
                ok
            }
            ("GET", "/api/transfers") => match svc.mounted() {
                Ok(m) => json!({ "transfers": m.transfers() }),
                Err(_) => json!({ "transfers": [] }),
            },
            ("GET", "/api/browse") => {
                let rel = clean_rel(&param(&query, "path").unwrap_or_default())?;
                serde_json::to_value(svc.mounted()?.browse(&rel).await?)?
            }
            ("POST", "/api/pin") => {
                let r: PathReq = serde_json::from_value(body)?;
                svc.mounted()?.pin(&clean_rel(&r.path)?, r.on.unwrap_or(true)).await?;
                ok
            }
            ("POST", "/api/free") => {
                let r: PathReq = serde_json::from_value(body)?;
                let freed = svc.mounted()?.free(&clean_rel(&r.path)?).await?;
                json!({ "ok": true, "freed": freed })
            }
            ("POST", "/api/refresh") => {
                let r: PathReq = serde_json::from_value(body)?;
                svc.mounted()?.refresh(&clean_rel(&r.path)?).await?;
                ok
            }
            ("POST", "/api/open") => {
                let r: PathReq = serde_json::from_value(body)?;
                let rel = clean_rel(&r.path)?;
                let mut target = svc.config().ok_or_else(|| anyhow!("not configured"))?.mountpoint();
                target.extend(rel.split('/').filter(|s| !s.is_empty()));
                open_path(&target.to_string_lossy())?;
                ok
            }
            ("POST", "/api/quit") => {
                svc.quit.notify_one();
                ok
            }
            _ => return Ok(json_resp(StatusCode::NOT_FOUND, &json!({ "error": "unknown endpoint" }))),
        };
        Ok(json_resp(StatusCode::OK, &out))
    }

    fn state(&self) -> Value {
        let svc = &self.svc;
        let cfg = svc.config();
        let config = cfg.as_ref().and_then(|c| serde_json::to_value(c).ok()).map(|mut v| {
            if let Some(o) = v.as_object_mut() {
                o.remove("password");
            }
            v
        });
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
            "configured": cfg.is_some(),
            "config": config,
            "password_known": svc.password_known(),
            "password_in_file": cfg.as_ref().is_some_and(|c| c.password.is_some()),
            "phase": svc.phase(),
            "error": svc.error(),
            "info": svc.current().map(|m| m.info()),
            "mountpoint": cfg.as_ref().map(|c| c.mountpoint().display().to_string()),
            "config_path": svc.cfg_path.display().to_string(),
            "default_mountpoint": crate::default_mountpoint(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_paths() {
        assert_eq!(param("path=F%C3%A9rias%202026%2Fa+b&x=1", "path").as_deref(), Some("Férias 2026/a b"));
        assert_eq!(param("x=1", "path"), None);
        assert_eq!(clean_rel("/a//b/").unwrap(), "a/b");
        assert_eq!(clean_rel("").unwrap(), "");
        assert!(clean_rel("a/../b").is_err());
        assert!(clean_rel("./a").is_err());
    }

    #[test]
    fn tokens() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(same(a.as_bytes(), a.clone().as_bytes()) && !same(a.as_bytes(), b.as_bytes()) && !same(b"ab", b"abc"));
    }
}
