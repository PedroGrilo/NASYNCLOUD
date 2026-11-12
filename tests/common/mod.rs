//! In-process WebDAV server (dav-server on a temp folder) with a few knobs to
//! simulate an unreachable or misbehaving NAS.
#![allow(dead_code)]

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dav_server::{body::Body, fakels::FakeLs, localfs::LocalFs, DavHandler};
use hyper::{server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

pub const USER: &str = "user";
pub const PASS: &str = "pass";
const AUTH: &str = "Basic dXNlcjpwYXNz";

pub struct TestServer {
    pub url: String,
    pub root: tempfile::TempDir,
    /// Simulates the NAS being unreachable.
    pub down: Arc<AtomicBool>,
    /// Makes every PUT fail with a 500.
    pub fail_put: Arc<AtomicBool>,
    /// Artificial latency added to every request.
    pub delay_ms: Arc<AtomicU64>,
    /// "METHOD /path [range]" for every request served.
    pub log: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestServer {
    pub async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dav = DavHandler::builder().filesystem(LocalFs::new(root.path(), false, false, false)).locksystem(FakeLs::new()).build_handler();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let down = Arc::new(AtomicBool::new(false));
        let fail_put = Arc::new(AtomicBool::new(false));
        let delay_ms = Arc::new(AtomicU64::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (down2, fail2, delay2, log2) = (down.clone(), fail_put.clone(), delay_ms.clone(), log.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                if down2.load(Ordering::SeqCst) {
                    continue; // drop the connection on the floor
                }
                let (dav, down, fail_put, delay_ms, log) = (dav.clone(), down2.clone(), fail2.clone(), delay2.clone(), log2.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (dav, down, fail_put, delay_ms, log) = (dav.clone(), down.clone(), fail_put.clone(), delay_ms.clone(), log.clone());
                        async move {
                            let reply = |code: u16| Ok::<_, Infallible>(Response::builder().status(code).body(Body::empty()).unwrap());
                            if down.load(Ordering::SeqCst) {
                                return reply(503);
                            }
                            if req.headers().get("authorization").and_then(|v| v.to_str().ok()) != Some(AUTH) {
                                return reply(401);
                            }
                            let range = req.headers().get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                            log.lock().unwrap().push(format!("{} {} {}", req.method(), req.uri().path(), range).trim().to_string());
                            let delay = delay_ms.load(Ordering::SeqCst);
                            if delay > 0 {
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                            }
                            if req.method() == "PUT" && fail_put.load(Ordering::SeqCst) {
                                return reply(500);
                            }
                            Ok(dav.handle(req).await)
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        Self { url, root, down, fail_put, delay_ms, log, task }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    pub fn write(&self, rel: &str, data: &[u8]) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    pub fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path(rel)).unwrap()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Number of logged requests whose line starts with `prefix` (e.g. "GET /a").
    pub fn count(&self, prefix: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).count()
    }

    pub fn clear_log(&self) {
        self.log.lock().unwrap().clear();
    }

    /// Names in a remote folder, sorted (handy for assertions).
    pub fn names(&self, rel: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(rel)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
}

/// Deterministic pseudo-random bytes.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Polls `cond` until it holds or `secs` elapse.
pub async fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}
