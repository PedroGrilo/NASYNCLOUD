//! A stand-in NAS: serves a local folder over WebDAV (dav-server) with Basic auth.
//! Used by `tests/e2e.sh` when rclone is not installed (e.g. on macOS).
//!
//!   cargo run --example fake_nas -- [--closed-top] <folder> [addr] [user] [pass]
//!
//! Defaults: 127.0.0.1:5005, pedro / segredo (the same as the e2e and demo scripts).
//!
//! `--closed-top` behaves like UGOS when the address is the NAS itself
//! (`http://nas:5005/`, no shared folder): the top is only the list of the
//! shared folders. Nothing can be created, replaced, renamed or removed there
//! (405), while everything works inside the folders that already exist.

use std::convert::Infallible;

use dav_server::{body::Body, fakels::FakeLs, localfs::LocalFs, DavHandler};
use hyper::{server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Whether `--closed-top` refuses this request: one that would change what
/// is at the top (`/name`), as source or as destination.
fn changes_top(method: &str, path: &str, destination: Option<&str>) -> bool {
    let at_top = |p: &str| p.split('/').filter(|s| !s.is_empty()).count() == 1;
    match method {
        "MKCOL" | "PUT" | "DELETE" | "PROPPATCH" | "LOCK" => at_top(path),
        // The destination is a full URL (`http://host/a/b`) or just its path.
        "MOVE" | "COPY" => at_top(path) || destination.is_some_and(|d| at_top(d.split_once("://").map_or(d, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p)))),
        _ => false,
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let (flags, args): (Vec<String>, Vec<String>) = std::env::args().skip(1).partition(|a| a.starts_with("--"));
    let closed_top = flags.iter().any(|f| f == "--closed-top");
    let Some(root) = args.first().filter(|_| flags.iter().all(|f| f == "--closed-top")) else {
        eprintln!("usage: fake_nas [--closed-top] <folder> [addr] [user] [pass]");
        std::process::exit(2);
    };
    let addr = args.get(1).map_or("127.0.0.1:5005", String::as_str);
    let user = args.get(2).map_or("pedro", String::as_str);
    let pass = args.get(3).map_or("segredo", String::as_str);
    let auth = format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()));

    let dav = DavHandler::builder().filesystem(LocalFs::new(root, false, false, false)).locksystem(FakeLs::new()).build_handler();
    let listener = TcpListener::bind(addr).await?;
    eprintln!("fake_nas: serving {root} on http://{addr}{}", if closed_top { " (closed top)" } else { "" });
    loop {
        let (stream, _) = listener.accept().await?;
        let (dav, auth) = (dav.clone(), auth.clone());
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let (dav, auth) = (dav.clone(), auth.clone());
                async move {
                    let range = req.headers().get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    // Same shape as rclone's access log, so the e2e greps work with either server.
                    let path = percent_encoding::percent_decode_str(req.uri().path()).decode_utf8_lossy().into_owned();
                    eprintln!("{} {} {}", req.method(), path, range);
                    if req.headers().get("authorization").and_then(|v| v.to_str().ok()) != Some(auth.as_str()) {
                        let resp = Response::builder().status(401).header("www-authenticate", "Basic realm=\"nas\"").body(Body::empty()).unwrap();
                        return Ok::<_, Infallible>(resp);
                    }
                    let destination = req.headers().get("destination").and_then(|v| v.to_str().ok()).map(|d| percent_encoding::percent_decode_str(d).decode_utf8_lossy().into_owned());
                    if closed_top && changes_top(req.method().as_str(), &path, destination.as_deref()) {
                        return Ok(Response::builder().status(405).body(Body::empty()).unwrap());
                    }
                    Ok(dav.handle(req).await)
                }
            });
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}
